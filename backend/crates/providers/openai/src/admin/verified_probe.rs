//! 铸票与复用验证共享一个候选出口，只有完整通过才发布运行态。
use super::*;
use crate::turn_state::VerifiedProbeBundle;
use gateway_admin::model::turn_state::{
    TurnStateProbeCompression, TurnStateProbePolicy, TurnStateProbeRequest, TurnStateSuccessRules,
};
use gateway_protocol::openai::sse::SseEventDecoder;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

struct Exchange {
    status: Option<u16>,
    headers: HeaderMap,
    document: Option<Value>,
    error: Option<&'static str>,
    request_headers: Vec<TurnStateProbeHeader>,
    http_version: Option<String>,
}

fn apply_headers(
    headers: &mut HeaderMap,
    request: &TurnStateProbeRequest,
) -> Result<(), ProviderAdminError> {
    for item in &request.extra_headers {
        let name = HeaderName::from_bytes(item.name.trim().as_bytes())
            .map_err(|_| provider_admin_error(ProviderAdminErrorKind::Invalid))?;
        let mut value = HeaderValue::from_str(&item.value)
            .map_err(|_| provider_admin_error(ProviderAdminErrorKind::Invalid))?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(())
}

fn response_document(body: &str) -> Result<Value, &'static str> {
    if let Ok(document) = serde_json::from_str::<Value>(body) {
        return Ok(document);
    }
    let mut decoder = SseEventDecoder::default();
    let mut events = decoder.push(body.as_bytes()).map_err(|_| "SSE 格式错误")?;
    events.extend(decoder.finish().map_err(|_| "SSE 流不完整")?);
    let mut document = None;
    for event in events {
        if event.data == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&event.data).map_err(|_| "SSE 事件格式错误")?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or_default();
        if matches!(kind, "error" | "response.failed" | "response.incomplete") {
            return Err("上游生成失败或不完整");
        }
        if kind == "response.completed" {
            document = value.get("response").cloned();
        }
    }
    document.ok_or("响应未包含完成事件")
}

fn success(
    exchange: &Exchange,
    rules: &TurnStateSuccessRules,
    model: &str,
) -> Result<(), &'static str> {
    if let Some(error) = exchange.error {
        return Err(error);
    }
    let status = exchange.status.ok_or("未收到上游响应")?;
    if status < rules.status_min || status > rules.status_max {
        return Err("HTTP 状态不符合条件");
    }
    let value = exchange.document.as_ref().ok_or("响应正文无法验证")?;
    let value = value.get("response").unwrap_or(value);
    if value.get("error").is_some_and(|v| !v.is_null())
        || matches!(
            value.get("status").and_then(Value::as_str),
            Some("failed" | "incomplete" | "cancelled")
        )
    {
        return Err("上游生成失败或不完整");
    }
    if rules.require_completed && value.get("status").and_then(Value::as_str) != Some("completed") {
        return Err("响应未完成");
    }
    if rules.require_model_match {
        let served = value
            .get("model")
            .and_then(Value::as_str)
            .ok_or("响应缺少实际模型")?;
        if (rules.expected_models.is_empty() && served != model)
            || (!rules.expected_models.is_empty()
                && !rules.expected_models.iter().any(|m| m == served))
        {
            return Err("实际模型不符合条件");
        }
    }
    for predicate in &rules.json_predicates {
        if !value
            .pointer(&predicate.pointer)
            .is_some_and(|v| predicate.values.contains(v))
        {
            return Err("响应字段不符合条件");
        }
    }
    Ok(())
}

async fn exchange(
    client: &reqwest::Client,
    url: &str,
    mut headers: HeaderMap,
    template: &TurnStateProbeRequest,
    model: &str,
    timeout: Duration,
    capture: bool,
) -> Exchange {
    let mut result = Exchange {
        status: None,
        headers: HeaderMap::new(),
        document: None,
        error: None,
        request_headers: Vec::new(),
        http_version: None,
    };
    let operation = async {
        let value = template.render_body(model);
        // 缓存归属必须与实际请求模型一致，自定义模板不能把其他模型的票写入当前键。
        if value.get("model").and_then(Value::as_str) != Some(model) {
            return Err("模板模型与本轮模型不一致");
        }
        let mut body = serde_json::to_vec(&value).map_err(|_| "请求体编码失败")?;
        if template.compression == TurnStateProbeCompression::Zstd {
            body = zstd::stream::encode_all(std::io::Cursor::new(body), template.compression_level)
                .map_err(|_| "请求体压缩失败")?;
            headers.insert(
                reqwest::header::CONTENT_ENCODING,
                HeaderValue::from_static("zstd"),
            );
        }
        apply_headers(&mut headers, template).map_err(|_| "请求头格式错误")?;
        let request = client
            .post(url)
            .headers(headers)
            .body(body)
            .build()
            .map_err(|_| "请求构造失败")?;
        if capture {
            result.request_headers = captured_headers(request.headers());
        }
        let response = client
            .execute(request)
            .await
            .map_err(|_| "代理连接或发送失败")?;
        result.status = Some(response.status().as_u16());
        result.headers = response.headers().clone();
        result.http_version = Some(format!("{:?}", response.version()));
        let body = read_capped_response_body(response, template.max_response_body_bytes as usize)
            .await
            .map_err(|_| "响应读取失败")?;
        if body.limit_exceeded() {
            return Err("响应正文超过上限");
        }
        result.document = Some(response_document(&body.into_string())?);
        Ok(())
    };
    result.error = match tokio::time::timeout(timeout, operation).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(_) => Some("请求超时"),
    };
    result
}

fn minted_cookies(
    headers: &HeaderMap,
    names: &[String],
    ttl: u64,
) -> Result<(HeaderValue, SystemTime), &'static str> {
    let now = SystemTime::now();
    let mut values = std::collections::BTreeMap::new();
    for raw in headers.get_all(reqwest::header::SET_COOKIE) {
        let Ok(raw) = raw.to_str() else {
            continue;
        };
        let Ok(cookie) = cookie::Cookie::parse(raw) else {
            continue;
        };
        if !names.iter().any(|name| name == cookie.name()) {
            continue;
        }
        let expires = if let Some(age) = cookie.max_age() {
            if age.whole_seconds() <= 0 {
                values.remove(cookie.name());
                continue;
            }
            now + Duration::from_secs(age.whole_seconds() as u64)
        } else if let Some(date) = cookie.expires_datetime() {
            if date.unix_timestamp() <= 0 {
                values.remove(cookie.name());
                continue;
            }
            SystemTime::UNIX_EPOCH + Duration::from_secs(date.unix_timestamp() as u64)
        } else {
            now + Duration::from_secs(ttl)
        };
        if expires <= now || cookie.value().is_empty() {
            values.remove(cookie.name());
            continue;
        }
        values.insert(
            cookie.name().to_owned(),
            (cookie.value().to_owned(), expires),
        );
    }
    let mut pairs = Vec::new();
    let mut expires = now + Duration::from_secs(ttl);
    for name in names {
        let (value, expiration) = values.get(name).ok_or("铸票未取得必需 Cookie")?;
        pairs.push(format!("{name}={value}"));
        expires = expires.min(*expiration);
    }
    let mut value = HeaderValue::from_str(&pairs.join("; ")).map_err(|_| "Cookie 格式错误")?;
    value.set_sensitive(true);
    Ok((value, expires))
}

impl OpenAiAdminProvider {
    pub(super) async fn probe_verified(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
        targets: Vec<TurnStateProbeTarget>,
        trigger: TurnStateSource,
        policy: TurnStateProbePolicy,
        publish: bool,
    ) -> Result<TurnStateProbeResult, ProviderAdminError> {
        let active_policy = self.turn_states.policy();
        // 草稿共用独立的预算与并发门，不得切换业务策略或清理已验证组合。
        let turn_states = if publish {
            &self.turn_states
        } else {
            &self.draft_turn_states
        };
        let started_at = Utc::now();
        let mut result = empty_turn_state_probe_result(account_id, model, trigger, started_at);
        if trigger == TurnStateSource::ManualProbe && !policy.manual_enabled {
            return Err(provider_admin_error(ProviderAdminErrorKind::Conflict)
                .with_public_message("手动状态探测已关闭"));
        }
        if (trigger == TurnStateSource::ManualProbe && !policy.manual_enabled)
            || (trigger == TurnStateSource::AutomaticRenewal && !policy.automatic_enabled)
        {
            return Ok(result);
        }
        let account = self.account(account_id).await?;
        if account.authentication_kind() != crate::credential::CODEX_AUTHENTICATION_KIND_OAUTH {
            return Err(provider_admin_error(ProviderAdminErrorKind::Unsupported));
        }
        let targets: Vec<_> = targets
            .into_iter()
            .filter(|target| target.proxy.is_some())
            .collect();
        if targets.is_empty() {
            return Err(provider_admin_error(ProviderAdminErrorKind::Invalid)
                .with_public_message("State 探测必须选择代理"));
        }
        let mut run = match turn_states.begin_probe(
            account_id,
            model,
            account.revision(),
            crate::turn_state::TurnStateProbeCandidates {
                targets,
                policy: policy.clone(),
            },
            account.outbound_proxy(),
            trigger,
        ) {
            TurnStateProbeAdmission::Ready(run) => run,
            _ if trigger == TurnStateSource::AutomaticRenewal => return Ok(result),
            _ => {
                return Err(provider_admin_error(ProviderAdminErrorKind::Conflict)
                    .with_public_message("探测忙碌、冷却中或预算不足"));
            }
        };
        let repository = crate::credential::CodexCredentialRepository::new(self.accounts.clone());
        let credential = repository
            .load_runtime_credential(&account)
            .await
            .map_err(|_| provider_admin_error(ProviderAdminErrorKind::CredentialRefreshRequired))?;
        let authorization = credential
            .authentication
            .authorization_header()
            .map_err(|_| provider_admin_error(ProviderAdminErrorKind::CredentialRefreshRequired))?;
        let mut base = crate::transport::headers::build_codex_model_headers(
            &self.profile.snapshot(),
            authorization.expose_secret(),
            account.upstream_account_id(),
        )
        .map_err(|_| provider_admin_error(ProviderAdminErrorKind::Internal))?;
        base.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        crate::transport::headers::insert_fedramp_header(&mut base, credential.is_fedramp_account);
        base.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        base.remove(reqwest::header::COOKIE);
        base.remove("x-codex-turn-state");
        let reuse_count = if policy.verification.mode
            == gateway_admin::model::turn_state::TurnStateVerificationMode::AcquireOnly
        {
            0
        } else {
            policy.verification.reuse_count
        };
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(policy.verification.round_timeout_seconds);
        let mut failures = Vec::new();
        let mut acquired = None;
        let url = endpoint_url(&self.base_url, CODEX_RESPONSES_PATH);
        'candidates: for target in run
            .candidates()
            .to_vec()
            .into_iter()
            .take(usize::from(policy.candidate_limit))
        {
            let Some(proxy) = target.proxy.as_ref() else {
                continue;
            };
            let client = match build_account_http_client(account_id.as_str(), Some(proxy)) {
                Ok(client) => client,
                Err(_) => {
                    failures.push(TurnStateProbeFailure {
                        target_id: target.id.clone(),
                        kind: TurnStateProbeFailureKind::ProxyConfiguration,
                    });
                    continue;
                }
            };
            let staged = ResponseHeaderCarryStore::new();
            staged.set_policy(policy.response_header_carry.clone());
            let context = ResponseHeaderContext {
                account_id,
                credential_revision: account.revision(),
                model,
                proxy: Some(proxy),
            };
            let mut frozen = HeaderMap::new();
            let mut minted_state = None;
            let mut expires_at = SystemTime::now() + Duration::from_secs(policy.state.ttl_seconds);
            let mut candidate_ok = true;
            for step in 0..=reuse_count {
                if step > 0 {
                    let delay = if step == 1 {
                        policy.verification.mint_to_reuse_delay_milliseconds
                    } else {
                        policy.verification.reuse_spacing_milliseconds
                    };
                    if tokio::time::timeout_at(
                        deadline,
                        tokio::time::sleep(Duration::from_millis(delay)),
                    )
                    .await
                    .is_err()
                    {
                        break 'candidates;
                    }
                }
                let permit = loop {
                    if tokio::time::Instant::now() >= deadline {
                        break 'candidates;
                    }
                    let admission = tokio::time::timeout_at(
                        deadline,
                        turn_states.start_probe_request(&run, &target),
                    )
                    .await;
                    match admission {
                        Ok(TurnStateProbeRequestAdmission::Ready(permit)) => break permit,
                        Ok(TurnStateProbeRequestAdmission::Deferred { until }) => {
                            if until >= deadline {
                                break 'candidates;
                            }
                            tokio::time::sleep_until(until).await;
                        }
                        _ => break 'candidates,
                    }
                };
                let current = self.account(account_id).await?;
                if current.revision() != account.revision()
                    || current.outbound_proxy() != account.outbound_proxy()
                    || self.turn_states.policy() != active_policy
                {
                    break 'candidates;
                }
                let mut headers = base.clone();
                if step > 0 {
                    headers.extend(frozen.clone());
                }
                let template = if step == 0 {
                    &policy.request
                } else {
                    &policy.reuse_request
                };
                apply_headers(&mut headers, template)?;
                // 显式覆盖可以用于诊断，但身份或票据不同的结果不能发布给当前业务上下文。
                let identity_matches = ["authorization", "chatgpt-account-id"]
                    .iter()
                    .all(|name| headers.get(*name) == base.get(*name))
                    && (step == 0
                        || ["cookie", "x-codex-turn-state"]
                            .iter()
                            .all(|name| headers.get(*name) == frozen.get(*name)));
                let request_id = Uuid::now_v7().to_string();
                headers.insert(
                    "x-client-request-id",
                    HeaderValue::from_str(&request_id)
                        .map_err(|_| provider_admin_error(ProviderAdminErrorKind::Internal))?,
                );
                let attempt_at = Utc::now();
                let attempt_start = Instant::now();
                let timeout = Duration::from_secs(policy.schedule.request_timeout_seconds)
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                let capture = self.turn_state_probe_capture.enabled();
                let response = exchange(
                    &client,
                    &url,
                    headers,
                    template,
                    model.as_str(),
                    timeout,
                    capture,
                )
                .await;
                drop(permit);
                let rules = if step == 0 {
                    &policy.verification.mint_success
                } else {
                    &policy.verification.reuse_success
                };
                let mut outcome = success(&response, rules, model.as_str());
                if outcome.is_ok() && !identity_matches {
                    outcome = Err("实际请求身份或票据与缓存上下文不一致");
                }
                if step == 0 && outcome.is_ok() {
                    let raw = crate::transport::response_header_carry_headers(&response.headers);
                    minted_state = TurnStateStore::state_from_headers_for_policy(&policy, &raw)
                        .or_else(|| {
                            response.document.as_ref().and_then(|v| {
                                TurnStateStore::state_from_json_for_policy(&policy, v)
                            })
                        });
                    if minted_state.is_none() {
                        outcome = Err("铸票未取得合法 State");
                    }
                    match minted_cookies(
                        &response.headers,
                        &policy.verification.required_cookie_names,
                        policy.state.ttl_seconds,
                    ) {
                        Ok((cookie, expiry)) => {
                            expires_at = expiry;
                            let token = staged.inject(&context, &mut frozen);
                            staged.capture(&context, token, gateway_admin::model::turn_state::ResponseHeaderCarrySource::TurnStateProbe, response.status.unwrap_or_default(), &raw);
                            staged.inject(&context, &mut frozen);
                            if !policy.verification.required_cookie_names.is_empty() {
                                frozen.insert(reqwest::header::COOKIE, cookie);
                            }
                            if let Some(state) = &minted_state
                                && let Ok(mut value) = HeaderValue::from_str(state)
                            {
                                value.set_sensitive(true);
                                frozen.insert("x-codex-turn-state", value);
                            }
                            for rule in &policy.response_header_carry.rules {
                                if rule.enabled && frozen.contains_key(&rule.target_header) {
                                    expires_at = expires_at.min(
                                        SystemTime::now() + Duration::from_secs(rule.ttl_seconds),
                                    );
                                }
                            }
                        }
                        Err(error) => outcome = Err(error),
                    }
                }
                if step > 0 && expires_at <= SystemTime::now() {
                    outcome = Err("验证期间 Cookie 或 State 已过期");
                }
                let latency_ms =
                    u64::try_from(attempt_start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let phase = if step == 0 {
                    "铸票".to_owned()
                } else {
                    format!("复用验证 {step}/{}", policy.verification.reuse_count)
                };
                let message = match outcome {
                    Ok(()) => format!("{phase}通过"),
                    Err(reason) => format!("{phase}失败：{reason}"),
                };
                let exchange_id = if capture {
                    let id = Uuid::now_v7().to_string();
                    self.turn_state_probe_capture
                        .try_capture(TurnStateProbeExchangeCapture {
                            id: id.clone(),
                            trigger,
                            account_id: account_id.as_str().to_owned(),
                            model: model.as_str().to_owned(),
                            target_id: target.id.clone(),
                            target_label: target.label.clone(),
                            request_id,
                            started_at: attempt_at,
                            finished_at: Utc::now(),
                            status_code: response.status,
                            http_version: response.http_version,
                            outcome: if outcome.is_ok() {
                                "validated"
                            } else {
                                "validation_failed"
                            }
                            .to_owned(),
                            latency_ms,
                            request_headers: response.request_headers,
                            response_headers: captured_headers(&response.headers),
                        })
                        .then_some(id)
                } else {
                    None
                };
                result.attempts.push(TurnStateProbeAttempt {
                    exchange_id,
                    target_id: target.id.clone(),
                    target_label: target.label.clone(),
                    success: outcome.is_ok(),
                    status_code: response.status,
                    latency_ms,
                    state_acquired: minted_state.is_some(),
                    message,
                });
                if outcome.is_err() {
                    candidate_ok = false;
                    let kind = match response.status {
                        Some(401 | 403) => TurnStateProbeFailureKind::AccountAuthenticationRejected,
                        Some(status) if !(200..300).contains(&status) => {
                            TurnStateProbeFailureKind::HttpStatus(status)
                        }
                        _ if response.error == Some("请求超时") => {
                            TurnStateProbeFailureKind::Timeout
                        }
                        None => TurnStateProbeFailureKind::Network,
                        _ => TurnStateProbeFailureKind::MissingState,
                    };
                    failures.push(TurnStateProbeFailure {
                        target_id: target.id.clone(),
                        kind,
                    });
                    if matches!(response.status, Some(401 | 403 | 429)) {
                        break 'candidates;
                    }
                    if step == 0 || policy.verification.stop_on_first_failure {
                        break;
                    }
                }
                if step == reuse_count && candidate_ok {
                    // 普通业务沿用验证模板的身份头，Cookie 与 State 始终来自铸票快照。
                    apply_headers(&mut frozen, &policy.reuse_request)?;
                    run.verified = Some(VerifiedProbeBundle {
                        id: Uuid::now_v7().to_string(),
                        headers: frozen.clone(),
                        proxy: proxy.clone(),
                        expires_at,
                        policy: policy.clone(),
                    });
                    acquired = minted_state.take();
                    result.active_target_id = Some(target.id.clone());
                }
            }
            if acquired.is_some() {
                break;
            }
        }
        result.finished_at = Utc::now();
        if !publish {
            result.active_target_id = None;
            return Ok(result);
        }
        let current_facts_match = self.account(account_id).await.is_ok_and(|current| {
            current.revision() == account.revision()
                && current.outbound_proxy() == account.outbound_proxy()
        });
        let result =
            turn_states.finish_probe(&mut run, result, acquired, &failures, current_facts_match);
        Ok(result)
    }
}
