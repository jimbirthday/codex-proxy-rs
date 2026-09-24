//! 自由探测的 Responses WebSocket 预热。对齐官方 `generate=false` 连接准备，不走 HTTP 铸票。

use std::time::Duration;

use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use gateway_admin::model::proxies::HttpProbeHeader;
use gateway_admin::model::turn_state::TurnStateSource;
use gateway_admin::ports::provider::{ProviderAdminError, ProviderAdminErrorKind};
use gateway_core::account::{OutboundProxy, ProviderAccountId};
use gateway_core::routing::UpstreamModelId;
use secrecy::ExposeSecret as _;
use serde_json::{Map, Value};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use super::{OpenAiAdminProvider, provider_admin_error};
use crate::transport::protocol::websocket::{
    websocket_metadata_turn_state, websocket_response_completed_id,
};
use crate::transport::websocket::{CodexWebSocketConnection, connect_responses_websocket};
const PREWARM_BETA: &str = "responses_websockets=2026-02-06";
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub(super) struct WebsocketPrewarmExchange {
    pub method: String,
    pub url: String,
    pub request_headers: Vec<HttpProbeHeader>,
    pub request_body: Vec<u8>,
    pub status_code: Option<u16>,
    pub response_headers: Vec<HttpProbeHeader>,
    pub body: Vec<u8>,
    pub error: Option<String>,
    pub turn_state_length: Option<u16>,
    pub turn_state_stored: bool,
}

impl OpenAiAdminProvider {
    pub(super) async fn websocket_prewarm(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
        proxy: Option<&OutboundProxy>,
        timeout_seconds: u64,
        body_override: Option<Map<String, Value>>,
    ) -> Result<WebsocketPrewarmExchange, ProviderAdminError> {
        let account = self.account(account_id).await?;
        if account.authentication_kind() != crate::credential::CODEX_AUTHENTICATION_KIND_OAUTH {
            return Err(provider_admin_error(ProviderAdminErrorKind::Unsupported)
                .with_public_message("WebSocket 预热只支持 OpenAI OAuth 账号"));
        }
        let credential = crate::credential::CodexCredentialRepository::new(self.accounts.clone())
            .load_runtime_credential(&account)
            .await
            .map_err(|_| provider_admin_error(ProviderAdminErrorKind::CredentialRefreshRequired))?;
        let authorization = credential
            .authentication
            .authorization_header()
            .map_err(|_| provider_admin_error(ProviderAdminErrorKind::CredentialRefreshRequired))?;
        let profile = self.profile.snapshot();
        let mut headers = crate::transport::headers::build_codex_model_headers(
            &profile,
            authorization.expose_secret(),
            account.upstream_account_id(),
        )
        .map_err(|_| provider_admin_error(ProviderAdminErrorKind::Internal))?;
        crate::transport::headers::insert_fedramp_header(
            &mut headers,
            credential.is_fedramp_account,
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("openai-beta"),
            reqwest::header::HeaderValue::from_static(PREWARM_BETA),
        );
        // 官方 Responses Lite 模型才带这个头。探测目标经常是这类模型，带上它更容易拿到 292/332。
        headers.insert(
            reqwest::header::HeaderName::from_static("x-openai-internal-codex-responses-lite"),
            reqwest::header::HeaderValue::from_static("true"),
        );
        if !credential.cookies.is_empty() {
            let cookie = credential
                .cookies
                .iter()
                .map(|cookie| format!("{}={}", cookie.name, cookie.value.expose_secret()))
                .collect::<Vec<_>>()
                .join("; ");
            headers.insert(
                reqwest::header::COOKIE,
                reqwest::header::HeaderValue::from_str(&cookie)
                    .map_err(|_| provider_admin_error(ProviderAdminErrorKind::Internal))?,
            );
        }
        let key = base64::engine::general_purpose::STANDARD.encode(Uuid::now_v7().as_bytes());
        let business_headers = headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect::<Vec<_>>();
        let mut connection =
            CodexWebSocketConnection::responses(&self.base_url, &key, business_headers);
        connection.outbound_proxy = proxy.cloned();
        let request_headers = connection
            .headers()
            .iter()
            .map(|(name, value)| HttpProbeHeader {
                name: name.clone(),
                value: value.as_bytes().to_vec(),
            })
            .collect();
        let payload = prewarm_payload(model.as_str(), body_override);
        let payload_text = serde_json::to_string(&payload)
            .map_err(|_| provider_admin_error(ProviderAdminErrorKind::Internal))?;
        let request_body = payload_text.as_bytes().to_vec();
        let url = connection.endpoint().to_owned();
        // 0 在 HTTP 探测里表示不限时；WebSocket 预热必须有截止时间，避免管理连接一直挂起。
        let timeout = Duration::from_secs(if timeout_seconds == 0 {
            120
        } else {
            timeout_seconds
        });
        let deadline = tokio::time::Instant::now() + timeout;
        let connect =
            tokio::time::timeout_at(deadline, connect_responses_websocket(&connection)).await;
        let (mut socket, response) = match connect {
            Ok(Ok(opened)) => opened,
            Ok(Err(error)) => {
                return Ok(failed_exchange(
                    url,
                    request_headers,
                    request_body,
                    None,
                    Vec::new(),
                    format!("WebSocket 握手失败: {error}"),
                ));
            }
            Err(_) => {
                return Ok(failed_exchange(
                    url,
                    request_headers,
                    request_body,
                    None,
                    Vec::new(),
                    "WebSocket 握手超时".to_owned(),
                ));
            }
        };
        let status_code = response.status().as_u16();
        let mut response_headers = response
            .headers()
            .iter()
            .map(|(name, value)| HttpProbeHeader {
                name: name.as_str().to_owned(),
                value: value.as_bytes().to_vec(),
            })
            .collect::<Vec<_>>();
        let mut observed = handshake_turn_state(response.headers());
        // 101 只完成协议升级；必须发送 response.create，服务端才会执行预热并返回事件。
        let send_error = match tokio::time::timeout_at(
            deadline,
            socket.send(Message::Text(payload_text.into())),
        )
        .await
        {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("预热发送失败: {error}")),
            Err(_) => Some("预热发送超时".to_owned()),
        };
        if let Some(error) = send_error {
            return Ok(failed_exchange(
                url,
                request_headers,
                request_body,
                Some(status_code),
                response_headers,
                error,
            ));
        }
        let mut body = Vec::new();
        let mut route = RouteObservation::default();
        let result = read_until_terminal(
            &mut socket,
            deadline,
            &mut body,
            &mut response_headers,
            &mut observed,
            &mut route,
            |state| self.turn_states.accepts_state(state),
        )
        .await;
        let mut error = None;
        let downgraded = route.is_downgraded(model.as_str());
        if downgraded {
            // 312 是安全缓冲降到更快模型后的票，不能沿这条 previous_response_id 续写。
            observed = None;
        }
        match result {
            Ok(response_id)
                if downgraded
                    || response_id.is_some()
                        && !observed
                            .as_deref()
                            .is_some_and(|state| self.turn_states.accepts_state(state)) =>
            {
                let follow_up = if downgraded {
                    escape_downgrade_payload(model.as_str())
                } else {
                    follow_up_payload(
                        &payload,
                        response_id.as_deref().unwrap_or_default(),
                        observed.as_deref(),
                    )
                };
                let follow_up_text = serde_json::to_string(&follow_up)
                    .map_err(|_| provider_admin_error(ProviderAdminErrorKind::Internal))?;
                match tokio::time::timeout_at(
                    deadline,
                    socket.send(Message::Text(follow_up_text.into())),
                )
                .await
                {
                    Ok(Ok(())) => {
                        if downgraded {
                            route = RouteObservation::default();
                        }
                        error = read_until_terminal(
                            &mut socket,
                            deadline,
                            &mut body,
                            &mut response_headers,
                            &mut observed,
                            &mut route,
                            |state| self.turn_states.accepts_state(state),
                        )
                        .await
                        .err();
                        if route.is_downgraded(model.as_str()) {
                            observed = None;
                        }
                    }
                    Ok(Err(send_error)) => error = Some(format!("续写发送失败: {send_error}")),
                    Err(_) => error = Some("续写发送超时".to_owned()),
                }
            }
            Ok(_) => {}
            Err(read_error) => error = Some(read_error),
        }
        let _ = tokio::time::timeout_at(deadline, socket.send(Message::Close(None))).await;
        let turn_state_length = observed.as_ref().map(|value| value.len() as u16);
        let turn_state_stored = if let Some(state) = observed.clone() {
            !route.is_downgraded(model.as_str())
                && self.turn_states.accepts_state(&state)
                && self
                    .turn_states
                    .put(
                        account_id,
                        account.revision(),
                        model,
                        state,
                        TurnStateSource::UpstreamResponse,
                    )
                    .is_some()
        } else {
            false
        };
        Ok(WebsocketPrewarmExchange {
            method: "WEBSOCKET".to_owned(),
            url,
            request_headers,
            request_body,
            status_code: Some(status_code),
            response_headers,
            body,
            error,
            turn_state_length,
            turn_state_stored,
        })
    }
}

fn failed_exchange(
    url: String,
    request_headers: Vec<HttpProbeHeader>,
    request_body: Vec<u8>,
    status_code: Option<u16>,
    response_headers: Vec<HttpProbeHeader>,
    error: String,
) -> WebsocketPrewarmExchange {
    WebsocketPrewarmExchange {
        method: "WEBSOCKET".to_owned(),
        url,
        request_headers,
        request_body,
        status_code,
        response_headers,
        body: Vec::new(),
        error: Some(error),
        turn_state_length: None,
        turn_state_stored: false,
    }
}

pub(super) fn prewarm_payload(model: &str, override_body: Option<Map<String, Value>>) -> Value {
    let mut body = override_body.unwrap_or_else(|| {
        serde_json::json!({
            "instructions": "Reply with OK. Do not call tools.",
            "input": tool_turn_input(),
            "stream": true
        })
        .as_object()
        .cloned()
        .unwrap_or_default()
    });
    body.insert(
        "type".to_owned(),
        Value::String("response.create".to_owned()),
    );
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    body.insert("store".to_owned(), Value::Bool(false));
    body.insert("generate".to_owned(), Value::Bool(false));
    // Responses Lite 不支持并行工具调用；该字段必须显式为 false。
    body.insert("parallel_tool_calls".to_owned(), Value::Bool(false));
    ensure_lite_reasoning_context(&mut body);
    Value::Object(body)
}

/// Responses Lite 在开启内部 Lite 头时要求使用完整会话上下文。
/// 保留调用方传入的 reasoning.effort 等字段，只覆盖协议必需的 context。
fn ensure_lite_reasoning_context(body: &mut Map<String, Value>) {
    if !body.get("reasoning").is_some_and(Value::is_object) {
        body.insert("reasoning".to_owned(), serde_json::json!({}));
    }
    if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.insert("context".to_owned(), Value::String("all_turns".to_owned()));
    }
}

fn tool_turn_input() -> Value {
    serde_json::json!([
        {
            "type": "additional_tools",
            "role": "developer",
            "tools": [{
                "type": "namespace",
                "name": "codex",
                "description": "local tools",
                "tools": [{
                    "type": "function",
                    "name": "noop",
                    "description": "Do nothing.",
                    "strict": false,
                    "parameters": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                }]
            }]
        },
        {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "Reply with OK. Do not call tools."}]
        }
    ])
}

/// 空工具的闲聊预热会被安全缓冲改派到更快模型，对应票长是 312。
/// 重试必须新开一帧，并声明工具，避免沿用降级响应。
fn escape_downgrade_payload(model: &str) -> Value {
    serde_json::json!({
        "type": "response.create",
        "model": model,
        "store": false,
        "stream": true,
        "generate": false,
        "parallel_tool_calls": false,
        "instructions": "Reply with OK. Do not call tools.",
        "input": tool_turn_input(),
        "reasoning": {"context": "all_turns"},
        "client_metadata": {"x-codex-safety-buffering-enabled": "false"}
    })
}

#[derive(Debug, Default)]
struct RouteObservation {
    served_model: Option<String>,
    faster_model: Option<String>,
}

impl RouteObservation {
    fn is_downgraded(&self, requested: &str) -> bool {
        self.served_model
            .as_deref()
            .is_some_and(|served| served != requested)
            || self
                .faster_model
                .as_deref()
                .is_some_and(|faster| self.served_model.as_deref() == Some(faster))
    }
}

fn note_route(value: &Value, route: &mut RouteObservation) {
    if let Some(model) = value.pointer("/response/model").and_then(Value::as_str) {
        route.served_model = Some(model.to_owned());
    }
    let faster = value
        .pointer("/headers/x-codex-safety-buffering-faster-model")
        .and_then(Value::as_str);
    if let Some(faster) = faster {
        route.faster_model = Some(faster.to_owned());
    }
}

fn handshake_turn_state(headers: &tungstenite::http::HeaderMap) -> Option<String> {
    let mut values = headers.get_all("x-codex-turn-state").iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok().map(ToOwned::to_owned)
}

fn follow_up_payload(
    prewarm: &Value,
    previous_response_id: &str,
    turn_state: Option<&str>,
) -> Value {
    // 保留自定义预热参数；历史已由 previous_response_id 引用，不重复提交用户输入。
    let mut body = prewarm.clone();
    body["previous_response_id"] = Value::String(previous_response_id.to_owned());
    body["input"] = serde_json::json!([]);
    body["generate"] = Value::Bool(false);
    body["parallel_tool_calls"] = Value::Bool(false);
    if let Some(object) = body.as_object_mut() {
        ensure_lite_reasoning_context(object);
    }
    if let Some(turn_state) = turn_state {
        if !body.get("client_metadata").is_some_and(Value::is_object) {
            body["client_metadata"] = serde_json::json!({});
        }
        body["client_metadata"]["x-codex-turn-state"] = Value::String(turn_state.to_owned());
    }
    body
}

fn remember_state(
    state: String,
    observed: &mut Option<String>,
    headers: &mut Vec<HttpProbeHeader>,
    accepts: impl Fn(&str) -> bool,
) {
    let replace = match observed.as_deref() {
        Some(current) if accepts(current) => false,
        Some(_) => accepts(&state) || state.len() > observed.as_ref().map_or(0, String::len),
        None => true,
    };
    if !replace {
        return;
    }
    headers.push(HttpProbeHeader {
        name: "x-codex-turn-state".to_owned(),
        value: state.as_bytes().to_vec(),
    });
    *observed = Some(state);
}

fn observe_frame(
    value: &Value,
    observed: &mut Option<String>,
    headers: &mut Vec<HttpProbeHeader>,
    route: &mut RouteObservation,
    accepts: impl Fn(&str) -> bool,
) {
    note_route(value, route);
    let mut candidates = Vec::new();
    if let Some(state) = websocket_metadata_turn_state(value) {
        candidates.push(state);
    }
    for pointer in [
        "/current_turn_state",
        "/error/current_turn_state",
        "/response/current_turn_state",
    ] {
        if let Some(state) = value.pointer(pointer).and_then(Value::as_str) {
            candidates.push(state.to_owned());
        }
    }
    for state in candidates {
        remember_state(state, observed, headers, &accepts);
    }
}

async fn read_until_terminal<S>(
    socket: &mut S,
    deadline: tokio::time::Instant,
    body: &mut Vec<u8>,
    response_headers: &mut Vec<HttpProbeHeader>,
    observed: &mut Option<String>,
    route: &mut RouteObservation,
    accepts: impl Fn(&str) -> bool,
) -> Result<Option<String>, String>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("预热读取超时".to_owned());
        }
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| "预热读取超时".to_owned())?
            .ok_or_else(|| "预热完成前连接已关闭".to_owned())?
            .map_err(|error| format!("预热读取失败: {error}"))?;
        // 文本帧和 UTF-8 二进制帧使用同一套事件与终止判断。
        let bytes = match message {
            Message::Text(text) => text.as_bytes().to_vec(),
            Message::Binary(bytes) => bytes.to_vec(),
            Message::Close(_) => return Err("预热完成前连接已关闭".to_owned()),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
        };
        append_limited(body, &bytes)?;
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        observe_frame(&value, observed, response_headers, route, &accepts);
        match value.get("type").and_then(Value::as_str) {
            Some("response.completed") => return Ok(websocket_response_completed_id(&value)),
            Some(event @ ("error" | "response.failed" | "response.incomplete")) => {
                let detail = [
                    "/error/message",
                    "/response/error/message",
                    "/response/incomplete_details/reason",
                ]
                .into_iter()
                .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
                .unwrap_or(event);
                return Err(format!("预热上游错误 ({event}): {detail}"));
            }
            _ => {}
        }
    }
}

fn append_limited(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    let room = MAX_BODY_BYTES.saturating_sub(body.len());
    let take = chunk.len().min(room);
    body.extend_from_slice(&chunk[..take]);
    if take < chunk.len() || body.len() == MAX_BODY_BYTES {
        return Err("预热正文超过 1 MiB，已截断".to_owned());
    }
    body.push(b'\n');
    Ok(())
}
