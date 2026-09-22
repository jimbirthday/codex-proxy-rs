//! 统一账号目录与跨 Provider 动态分派。

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use futures::{StreamExt as _, future::BoxFuture};
use gateway_core::{
    account::ProviderAccountId,
    engine::probe::{AccountProbe, AccountProbeRequest},
    routing::{ProviderKind, UpstreamModelId},
    runtime::SnapshotControl,
    task::{ScheduledTask, WorkerCycleContext, WorkerTaskError},
};
use uuid::Uuid;

use crate::{
    model::{
        AdminError, MutationActor, MutationContext,
        accounts::{
            AccountConnectionTestEvent, AccountConnectionTestEventStream, AccountListQuery,
            AccountPageItem, AccountUpdateResult, AccountUsage, AccountUsageWindowQuery,
            AccountsUpdateResult, BatchUpdateAccounts, UpdateAccount,
        },
        auth::{AdminAuditEvent, AuditActorKind},
        observability::TimeRange,
        provider_credentials::{
            AccountDirectoryItem, AccountDirectoryPage, AccountExportBundle, AccountPersonalInfo,
            AccountRefreshResult, ConsumeProviderResetCredit, PrepareCredentialRefresh,
            ProviderModels, ProviderProfileAvatar, ProviderQuota, ProviderQuotaRequest,
            ProviderQuotaWindow, ProviderResetCreditResult, ProviderResetCredits,
            QuotaLocalUsageAttribution,
        },
        proxies::ProxyListQuery,
        quota_forecast::{AccountQuotaForecastReport, account_quota_forecasts},
        quota_forecast_sampling::{QuotaForecastPoint, select_forecast_sample},
        turn_state::{
            TurnStateOverviewEntry, TurnStateProbeResult, TurnStateProbeTarget, TurnStateSnapshot,
            TurnStateSource,
        },
        turn_state_capture::{
            TurnStateCaptureStatus, TurnStateProbeExchangeDetail, TurnStateProbeExchangePage,
            TurnStateProbeExchangeQuery,
        },
    },
    ports::{
        provider::ProviderAdminRegistry,
        proxy::ProxyStore,
        store::{AccountRuntimeStore, AccountStore, AuthStore},
        turn_state_capture::TurnStateProbeCaptureStore,
    },
};

use super::{
    commit_credential_refresh, map_provider_error, map_store_error, publish_committed,
    validate_prepared_rotation,
};

const CONNECTION_TEST_INPUT: &str = "Reply with exactly OK.";
pub(crate) const TURN_STATE_RENEWAL_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(10);

/// 统一账号页消费的服务。
#[async_trait]
pub trait AccountsService: Send + Sync {
    async fn free_probe(
        &self,
        _context: &MutationContext,
        _command: crate::model::proxies::FreeProbeCommand,
    ) -> Result<crate::model::proxies::FreeProbeSession, AdminError> {
        Err(AdminError::invalid("当前实例不支持自由探测"))
    }

    async fn free_probe_body(
        &self,
        _context: &MutationContext,
        _id: &str,
    ) -> Result<Arc<dyn crate::ports::proxy::HttpProbeBody>, AdminError> {
        Err(AdminError::not_found("探测正文不存在或已过期"))
    }

    async fn list(&self, query: AccountListQuery) -> Result<AccountDirectoryPage, AdminError>;

    async fn export(
        &self,
        context: &MutationContext,
        account_ids: Vec<ProviderAccountId>,
    ) -> Result<AccountExportBundle, AdminError>;

    async fn refresh(
        &self,
        context: &MutationContext,
        account_id: ProviderAccountId,
    ) -> Result<AccountRefreshResult, AdminError>;

    async fn recover(
        &self,
        context: &MutationContext,
        account_id: ProviderAccountId,
    ) -> Result<AccountRefreshResult, AdminError>;

    async fn update(
        &self,
        context: &MutationContext,
        command: UpdateAccount,
    ) -> Result<AccountUpdateResult, AdminError>;

    async fn lower_concurrency_limit(
        &self,
        context: &MutationContext,
        account_id: ProviderAccountId,
        limit: gateway_core::account::AccountConcurrencyLimit,
    ) -> Result<Option<AccountUpdateResult>, AdminError>;

    async fn batch_update(
        &self,
        context: &MutationContext,
        command: BatchUpdateAccounts,
    ) -> Result<AccountsUpdateResult, AdminError>;

    async fn account_configuration(
        &self,
        _account_id: &ProviderAccountId,
    ) -> Result<Option<crate::model::provider_credentials::ProviderDocument>, AdminError> {
        Ok(None)
    }

    async fn quota(
        &self,
        account_id: &ProviderAccountId,
        refresh: bool,
    ) -> Result<AccountDirectoryItem, AdminError>;

    async fn quota_forecast(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<AccountQuotaForecastReport, AdminError>;

    async fn personal_info(
        &self,
        _account_id: &ProviderAccountId,
    ) -> Result<AccountPersonalInfo, AdminError> {
        Err(AdminError::invalid("当前 Provider 不支持个人信息"))
    }

    async fn profile_avatar(
        &self,
        _account_id: &ProviderAccountId,
    ) -> Result<ProviderProfileAvatar, AdminError> {
        Err(AdminError::invalid("当前 Provider 不支持账号头像"))
    }

    async fn reset_credits(
        &self,
        _context: &MutationContext,
        _account_id: ProviderAccountId,
    ) -> Result<ProviderResetCredits, AdminError> {
        Err(AdminError::invalid("当前 Provider 不支持重置额度"))
    }

    async fn consume_reset_credit(
        &self,
        _context: &MutationContext,
        _command: ConsumeProviderResetCredit,
    ) -> Result<ProviderResetCreditResult, AdminError> {
        Err(AdminError::invalid("当前 Provider 不支持重置额度"))
    }

    async fn models(
        &self,
        account_id: &ProviderAccountId,
        refresh: bool,
    ) -> Result<ProviderModels, AdminError>;

    async fn test_connection(
        &self,
        account_id: ProviderAccountId,
        upstream_model: UpstreamModelId,
    ) -> Result<AccountConnectionTestEventStream, AdminError>;

    async fn turn_state_snapshot(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
    ) -> Result<Option<TurnStateSnapshot>, AdminError>;

    async fn turn_state_overview(&self) -> Result<Vec<TurnStateOverviewEntry>, AdminError>;

    async fn probe_turn_state(
        &self,
        account_id: ProviderAccountId,
        model: UpstreamModelId,
    ) -> Result<TurnStateProbeResult, AdminError>;

    async fn test_turn_state_policy(
        &self,
        _account_id: ProviderAccountId,
        _model: UpstreamModelId,
        _policy: crate::model::turn_state::TurnStateProbePolicy,
    ) -> Result<TurnStateProbeResult, AdminError> {
        Err(AdminError::invalid("当前服务不支持策略草稿测试"))
    }

    async fn turn_state_capture_status(&self) -> Result<TurnStateCaptureStatus, AdminError> {
        Err(AdminError::invalid("当前服务不支持探测报头采集"))
    }

    async fn start_turn_state_capture(
        &self,
        _context: &MutationContext,
        _duration: std::time::Duration,
    ) -> Result<TurnStateCaptureStatus, AdminError> {
        Err(AdminError::invalid("当前服务不支持探测报头采集"))
    }

    async fn stop_turn_state_capture(
        &self,
        _context: &MutationContext,
    ) -> Result<TurnStateCaptureStatus, AdminError> {
        Err(AdminError::invalid("当前服务不支持探测报头采集"))
    }

    async fn list_turn_state_probe_exchanges(
        &self,
        _query: TurnStateProbeExchangeQuery,
    ) -> Result<TurnStateProbeExchangePage, AdminError> {
        Err(AdminError::invalid("当前服务不支持探测报头采集"))
    }

    async fn turn_state_probe_exchange_detail(
        &self,
        _id: &str,
    ) -> Result<TurnStateProbeExchangeDetail, AdminError> {
        Err(AdminError::invalid("当前服务不支持探测报头采集"))
    }

    async fn reveal_turn_state_probe_exchange(
        &self,
        _context: &MutationContext,
        _id: &str,
    ) -> Result<TurnStateProbeExchangeDetail, AdminError> {
        Err(AdminError::invalid("当前服务不支持探测报头采集"))
    }
}

pub(crate) struct DefaultAccountsService {
    accounts: Arc<dyn AccountStore>,
    account_runtime: Arc<dyn AccountRuntimeStore>,
    providers: ProviderAdminRegistry,
    snapshot: Arc<dyn SnapshotControl>,
    probe: Arc<dyn AccountProbe>,
    proxies: Arc<dyn ProxyStore>,
    settings: Arc<dyn crate::ports::store::SettingsStore>,
    turn_state_probe_capture: Arc<dyn TurnStateProbeCaptureStore>,
    http_probe: Arc<dyn crate::ports::proxy::ProxyProbe>,
    http_probe_records: std::sync::Mutex<BTreeMap<String, HttpProbeRecord>>,
    auth: Arc<dyn AuthStore>,
    reset_credit_locks:
        Arc<futures::lock::Mutex<BTreeMap<ProviderAccountId, Arc<futures::lock::Mutex<()>>>>>,
}

struct MemoryProbeBody {
    bytes: Vec<u8>,
    finished_at: std::time::Instant,
}

#[async_trait]
impl crate::ports::proxy::HttpProbeBody for MemoryProbeBody {
    fn byte_length(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    fn finished_at(&self) -> Option<std::time::Instant> {
        Some(self.finished_at)
    }

    async fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>, AdminError> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start >= self.bytes.len() {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(length).min(self.bytes.len());
        Ok(self.bytes[start..end].to_vec())
    }
}

struct HttpProbeRecord {
    owner: MutationActor,
    body: Arc<dyn crate::ports::proxy::HttpProbeBody>,
}

fn prune_probe_records(records: &mut BTreeMap<String, HttpProbeRecord>) {
    let now = std::time::Instant::now();
    records.retain(|_, record| {
        record
            .body
            .finished_at()
            .is_none_or(|at| now.duration_since(at) < std::time::Duration::from_secs(30 * 60))
    });
}

async fn websocket_prewarm_probe(
    service: &DefaultAccountsService,
    context: &MutationContext,
    command: crate::model::proxies::FreeProbeCommand,
) -> Result<crate::model::proxies::FreeProbeSession, AdminError> {
    use crate::model::proxies::AccountProxySelection;
    let account_id = command
        .account_id
        .ok_or_else(|| AdminError::invalid("WebSocket 预热必须选择 OpenAI OAuth 账号"))?;
    let model = gateway_core::routing::UpstreamModelId::new(command.model.unwrap_or_default())
        .map_err(|_| AdminError::invalid("预热模型不合法"))?;
    let body = if command.request.body.is_empty() {
        None
    } else {
        Some(
            serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(
                &command.request.body,
            )
            .map_err(|_| AdminError::invalid("预热正文必须是 JSON 对象"))?,
        )
    };
    let proxy = match command.proxy {
        AccountProxySelection::Direct => None,
        AccountProxySelection::Url(proxy) => Some(proxy),
        AccountProxySelection::Saved(id) => Some(
            service
                .proxies
                .get(&id)
                .await
                .map_err(|error| map_store_error(error, "WebSocket prewarm proxy"))?
                .proxy,
        ),
    };
    let (_, provider) = service.provider_for_account(&account_id).await?;
    let result = provider
        .websocket_prewarm_probe(
            &account_id,
            &model,
            proxy.as_ref(),
            command.request.timeout_seconds,
            body,
        )
        .await
        .map_err(|error| {
            if let Some(message) = error.public_message() {
                return AdminError::invalid(message);
            }
            map_provider_error(error, "WebSocket prewarm")
        })?;
    let id = Uuid::now_v7().to_string();
    let stored = std::sync::Arc::new(MemoryProbeBody {
        bytes: result.body.clone(),
        finished_at: std::time::Instant::now(),
    });
    let preview = result
        .body
        .iter()
        .take(64 * 1024)
        .copied()
        .collect::<Vec<_>>();
    let received = u64::try_from(result.body.len()).unwrap_or(u64::MAX);
    let elapsed_ms = result.exchange.elapsed_ms;
    let error = result.exchange.error.clone();
    let events = futures::stream::iter([
        crate::model::proxies::HttpProbeEvent::Headers(Box::new(result.exchange)),
        crate::model::proxies::HttpProbeEvent::Progress {
            received_bytes: received,
            preview,
        },
        crate::model::proxies::HttpProbeEvent::Complete { elapsed_ms, error },
    ])
    .boxed();
    let mut records = service
        .http_probe_records
        .lock()
        .expect("probe records mutex");
    prune_probe_records(&mut records);
    if records.len() >= 32 {
        return Err(AdminError::unavailable(
            "同时运行的探测过多，请先停止部分请求",
        ));
    }
    records.insert(
        id.clone(),
        HttpProbeRecord {
            owner: context.actor.clone(),
            body: stored,
        },
    );
    Ok(crate::model::proxies::FreeProbeSession { id, events })
}

/// 账号诊断依赖 HTTP 执行、可选采集存储与管理员审计，集中注入。
pub(crate) struct AccountDiagnosticsDependencies {
    pub(crate) settings: Arc<dyn crate::ports::store::SettingsStore>,
    pub(crate) store: Arc<dyn TurnStateProbeCaptureStore>,
    pub(crate) auth: Arc<dyn AuthStore>,
    pub(crate) http_probe: Arc<dyn crate::ports::proxy::ProxyProbe>,
}

impl DefaultAccountsService {
    #[must_use]
    pub(crate) fn new(
        accounts: Arc<dyn AccountStore>,
        account_runtime: Arc<dyn AccountRuntimeStore>,
        providers: ProviderAdminRegistry,
        snapshot: Arc<dyn SnapshotControl>,
        probe: Arc<dyn AccountProbe>,
        proxies: Arc<dyn ProxyStore>,
        diagnostics: AccountDiagnosticsDependencies,
    ) -> Self {
        Self {
            accounts,
            account_runtime,
            providers,
            snapshot,
            probe,
            proxies,
            turn_state_probe_capture: diagnostics.store,
            settings: diagnostics.settings,
            auth: diagnostics.auth,
            http_probe: diagnostics.http_probe,
            http_probe_records: std::sync::Mutex::new(BTreeMap::new()),
            reset_credit_locks: Arc::new(futures::lock::Mutex::new(BTreeMap::new())),
        }
    }

    async fn reset_credit_lock(
        &self,
        account_id: &ProviderAccountId,
    ) -> Arc<futures::lock::Mutex<()>> {
        let mut locks = self.reset_credit_locks.lock().await;
        Arc::clone(
            locks
                .entry(account_id.clone())
                .or_insert_with(|| Arc::new(futures::lock::Mutex::new(()))),
        )
    }

    async fn append_probe_audit(
        &self,
        context: &MutationContext,
        action: &str,
        entity_kind: &str,
        entity_ref: &str,
        changed_fields: Vec<String>,
    ) -> Result<(), AdminError> {
        let (actor_kind, actor_admin_user_id, actor_ref) = match &context.actor {
            MutationActor::AdminSession { admin_user_id } => (
                AuditActorKind::AdminSession,
                Some(admin_user_id.clone()),
                crate::model::auth::admin_session_actor_ref(admin_user_id),
            ),
            MutationActor::AdminApiKey => (
                AuditActorKind::AdminApiKey,
                None,
                "admin_api_key".to_owned(),
            ),
            MutationActor::System => (AuditActorKind::System, None, "system".to_owned()),
        };
        self.auth
            .append_audit_event(AdminAuditEvent {
                id: format!("audit_{}", Uuid::now_v7().simple()),
                actor_kind,
                actor_admin_user_id,
                actor_ref,
                request_id: Some(context.request_id.clone()),
                action: action.to_owned(),
                entity_kind: entity_kind.to_owned(),
                entity_ref: entity_ref.to_owned(),
                config_revision: None,
                changed_fields,
                occurred_at: Utc::now(),
            })
            .await
            .map_err(|error| map_store_error(error, "turn state probe capture audit"))
    }

    async fn turn_state_probe_targets(&self) -> Result<Vec<TurnStateProbeTarget>, AdminError> {
        let mut targets = Vec::new();
        let mut page = 1;
        loop {
            let result = self
                .proxies
                .list(ProxyListQuery {
                    page,
                    page_size: crate::model::PageSize::new(200)
                        .map_err(|_| AdminError::invalid("代理分页大小不合法"))?,
                    search: String::new(),
                })
                .await
                .map_err(|error| map_store_error(error, "turn state proxy list"))?;
            let count = result.items.len();
            targets.extend(result.items.into_iter().map(|proxy| TurnStateProbeTarget {
                id: proxy.id,
                label: proxy.name,
                proxy: Some(proxy.proxy),
            }));
            if count < 200 {
                return if targets.is_empty() {
                    Err(AdminError::invalid("未配置可用于状态探测的代理"))
                } else {
                    Ok(targets)
                };
            }
            page = page.saturating_add(1);
        }
    }

    pub(crate) async fn renew_due_turn_states(&self) {
        // 与账号诊断生命周期一同清理临时正文，不让无人访问的记录长期占用磁盘。
        prune_probe_records(&mut self.http_probe_records.lock().expect("probe records mutex"));
        let concurrency = self
            .settings
            .load_turn_state_probe_policy()
            .await
            .ok()
            .map(|policy| usize::from(policy.schedule.max_concurrency).max(1))
            .unwrap_or(2);
        futures::stream::iter(self.providers.claim_due_turn_state_subjects())
            .for_each_concurrent(Some(concurrency), |subject| async move {
                // 排队期间业务响应可能已刷新 state；调用前重新核对，减少无效探测。
                let still_due =
                    self.providers.due_turn_state_subjects().iter().any(|due| {
                        due.account_id == subject.account_id && due.model == subject.model
                    });
                if !still_due {
                    return;
                }
                if let Err(error) = self
                    .probe_turn_state_with_source(
                        subject.account_id.clone(),
                        subject.model.clone(),
                        TurnStateSource::AutomaticRenewal,
                    )
                    .await
                {
                    tracing::warn!(
                        account_id = %subject.account_id,
                        model = %subject.model,
                        error = %error,
                        "Codex turn state 自动续采失败"
                    );
                }
            })
            .await;
    }

    async fn probe_turn_state_with_source(
        &self,
        account_id: ProviderAccountId,
        model: UpstreamModelId,
        source: TurnStateSource,
    ) -> Result<TurnStateProbeResult, AdminError> {
        let (_, provider) = self.provider_for_account(&account_id).await?;
        let policy = self
            .settings
            .load_turn_state_probe_policy()
            .await
            .map_err(|error| map_store_error(error, "turn state probe policy"))?;
        policy.validate()?;
        if source == TurnStateSource::ManualProbe && !policy.manual_enabled {
            return Err(AdminError::conflict("手动状态探测已关闭"));
        }
        if source == TurnStateSource::AutomaticRenewal && !policy.automatic_enabled {
            return Ok(TurnStateProbeResult {
                account_id: account_id.as_str().to_owned(),
                model: model.as_str().to_owned(),
                started_at: Utc::now(),
                finished_at: Utc::now(),
                trigger: source,
                active_target_id: None,
                state_expires_at: None,
                attempts: Vec::new(),
            });
        }
        // 完整目录用于同步冷却；策略只限制本轮允许使用的候选，不把未选中的代理当作已删除。
        let targets = self.turn_state_probe_targets().await?;
        provider
            .probe_turn_state(&account_id, &model, targets, source, policy)
            .await
            .map_err(|error| map_provider_error(error, "Codex turn state probe"))
    }

    async fn load_account(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<AccountPageItem, AdminError> {
        let runtime = self
            .account_runtime
            .account_runtime(&[account_id.as_str().to_owned()])
            .await
            .map_err(|error| map_store_error(error, "account runtime"))?;
        self.accounts
            .load_account(account_id.as_str(), runtime)
            .await
            .map_err(|error| map_store_error(error, "provider account"))?
            .ok_or_else(|| AdminError::not_found("Provider 账号不存在"))
    }

    async fn provider_for_account(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<
        (
            AccountPageItem,
            Arc<dyn crate::ports::provider::ProviderAdmin>,
        ),
        AdminError,
    > {
        let item = self.load_account(account_id).await?;
        let provider = self
            .providers
            .require(&item.account.provider_kind)
            .map_err(|error| map_provider_error(error, "provider account"))?;
        Ok((item, provider))
    }

    async fn attach_quota_local_usage(
        &self,
        accounts: &[AccountPageItem],
        quotas: &mut [ProviderQuota],
    ) -> Result<(), AdminError> {
        let windows = accounts
            .iter()
            .zip(quotas.iter())
            .flat_map(|(item, quota)| {
                quota
                    .windows
                    .iter()
                    .filter(|window| window.local_usage.is_none())
                    .filter_map(|window| quota_usage_window(&item.account.id, window))
            })
            .collect::<Vec<_>>();
        if windows.is_empty() {
            return Ok(());
        }
        let usage_by_window = self
            .accounts
            .load_account_usage_by_windows(&windows)
            .await
            .map_err(|error| map_store_error(error, "quota window usage"))?
            .into_iter()
            .map(|result| ((result.account_id, result.key), result.usage))
            .collect::<BTreeMap<_, _>>();
        for (item, quota) in accounts.iter().zip(quotas) {
            for window in &mut quota.windows {
                if window.local_usage.is_none()
                    && window.local_usage_attribution == QuotaLocalUsageAttribution::AccountWide
                {
                    let key = (item.account.id.clone(), window.key.clone());
                    if let Some(usage) = usage_by_window.get(&key) {
                        window.local_usage = Some(usage.clone());
                    }
                }
            }
        }
        Ok(())
    }

    async fn load_api_key_usage(
        &self,
        accounts: &[AccountPageItem],
    ) -> Result<BTreeMap<String, AccountUsage>, AdminError> {
        let now = Utc::now();
        // API Key 没有套餐周期；本地累计直接查询账号创建后仍保留的请求记录。
        let windows = accounts
            .iter()
            .filter(|item| item.account.authentication_kind == "api_key")
            .map(|item| AccountUsageWindowQuery {
                account_id: item.account.id.clone(),
                key: "account-lifetime".to_owned(),
                range: TimeRange {
                    start: item.account.created_at,
                    end: now,
                },
            })
            .collect::<Vec<_>>();
        if windows.is_empty() {
            return Ok(BTreeMap::new());
        }
        Ok(self
            .accounts
            .load_account_usage_by_windows(&windows)
            .await
            .map_err(|error| map_store_error(error, "API Key account usage"))?
            .into_iter()
            .map(|result| (result.account_id, result.usage))
            .collect())
    }

    async fn load_directory_item(
        &self,
        account_id: &ProviderAccountId,
        refresh_quota: bool,
    ) -> Result<AccountDirectoryItem, AdminError> {
        let (stored, provider) = self.provider_for_account(account_id).await?;
        let account = &stored.account;
        let now = Utc::now();
        let rolling_range = TimeRange {
            start: now - Duration::hours(24),
            end: now,
        };
        let ids = vec![account.id.clone()];
        let rolling_usage = self
            .accounts
            .load_account_usage(rolling_range, &ids)
            .await
            .map_err(|error| map_store_error(error, "rolling account usage"))?;
        let rolling_usage = rolling_usage.into_iter().next();
        let mut quota = provider
            .quota(ProviderQuotaRequest {
                account_id: account_id.clone(),
                refresh: refresh_quota,
                rolling_usage: rolling_usage.clone(),
            })
            .await
            .map_err(|error| map_provider_error(error, "provider quota"))?;
        let mut stored = if refresh_quota {
            self.load_account(account_id).await?
        } else {
            stored
        };
        self.attach_quota_local_usage(
            std::slice::from_ref(&stored),
            std::slice::from_mut(&mut quota),
        )
        .await?;
        let usage = self
            .load_api_key_usage(std::slice::from_ref(&stored))
            .await?
            .remove(&stored.account.id)
            .or_else(|| {
                quota
                    .usage_window()
                    .and_then(|(window, _)| window.local_usage.clone())
            });
        Ok(AccountDirectoryItem {
            plan_type_display: self.providers.resolve_account_plan(
                stored.account.provider_kind.as_str(),
                &mut stored.account.plan_type,
                Some(&quota),
            ),
            projection: stored.projection,
            usage,
            account: stored.account,
            quota,
        })
    }
}

#[async_trait]
impl AccountsService for DefaultAccountsService {
    async fn free_probe(
        &self,
        context: &MutationContext,
        mut command: crate::model::proxies::FreeProbeCommand,
    ) -> Result<crate::model::proxies::FreeProbeSession, AdminError> {
        use crate::model::proxies::AccountProxySelection;
        // 审计只保存动作，不包含 URL 查询参数、报头或正文中的敏感值。
        self.append_probe_audit(
            context,
            match command.mode {
                crate::model::proxies::FreeProbeMode::WebsocketPrewarm => "ws_prewarm.send",
                crate::model::proxies::FreeProbeMode::Http => "http_probe.send",
            },
            "http_probe",
            "manual",
            Vec::new(),
        )
        .await?;
        if command.mode == crate::model::proxies::FreeProbeMode::WebsocketPrewarm {
            return websocket_prewarm_probe(self, context, command).await;
        }
        if let Some(account_id) = &command.account_id {
            let (_, provider) = self.provider_for_account(account_id).await?;
            if command.use_account_headers {
                let defaults = provider
                    .http_probe_headers(account_id)
                    .await
                    .map_err(|error| map_provider_error(error, "HTTP probe credentials"))?;
                for header in defaults {
                    // 用户提供的同名报头（包括空值和重复值）优先，不能被账号模板覆盖。
                    if !command
                        .request
                        .headers
                        .iter()
                        .any(|value| value.name.eq_ignore_ascii_case(&header.name))
                    {
                        command.request.headers.push(header);
                    }
                }
            }
        } else if command.use_account_headers {
            return Err(AdminError::invalid("使用账号报头时必须选择账号"));
        }
        let proxy = match command.proxy {
            AccountProxySelection::Direct => None,
            AccountProxySelection::Url(proxy) => Some(proxy),
            AccountProxySelection::Saved(id) => Some(
                self.proxies
                    .get(&id)
                    .await
                    .map_err(|error| map_store_error(error, "HTTP probe proxy"))?
                    .proxy,
            ),
        };
        let session = self
            .http_probe
            .send_http(proxy.as_ref(), command.request)
            .await?;
        let id = Uuid::now_v7().to_string();
        let mut records = self.http_probe_records.lock().expect("probe records mutex");
        prune_probe_records(&mut records);
        // 保留最近 32 份诊断正文，运行中的请求不会被新请求挤掉。
        if records.len() >= 32 {
            let oldest = records
                .iter()
                .filter_map(|(id, record)| record.body.finished_at().map(|at| (id.clone(), at)))
                .min_by_key(|(_, at)| *at)
                .map(|(id, _)| id);
            if let Some(oldest) = oldest {
                records.remove(&oldest);
            } else {
                return Err(AdminError::unavailable(
                    "同时运行的探测过多，请先停止部分请求",
                ));
            }
        }
        records.insert(
            id.clone(),
            HttpProbeRecord {
                owner: context.actor.clone(),
                body: session.body,
            },
        );
        Ok(crate::model::proxies::FreeProbeSession {
            id,
            events: session.events,
        })
    }

    async fn free_probe_body(
        &self,
        context: &MutationContext,
        id: &str,
    ) -> Result<Arc<dyn crate::ports::proxy::HttpProbeBody>, AdminError> {
        let mut records = self.http_probe_records.lock().expect("probe records mutex");
        prune_probe_records(&mut records);
        records
            .get(id)
            .filter(|record| record.owner == context.actor)
            .map(|record| Arc::clone(&record.body))
            .ok_or_else(|| AdminError::not_found("探测正文不存在或已过期"))
    }

    async fn list(&self, query: AccountListQuery) -> Result<AccountDirectoryPage, AdminError> {
        let runtime = self
            .account_runtime
            .active_rate_limits()
            .await
            .map_err(|error| map_store_error(error, "account runtime"))?;
        let page = self
            .accounts
            .list_accounts(query, runtime)
            .await
            .map_err(|error| map_store_error(error, "account directory"))?;
        let now = Utc::now();
        let rolling_range = TimeRange {
            start: now - Duration::hours(24),
            end: now,
        };
        let ids = page
            .items
            .iter()
            .map(|item| item.account.id.clone())
            .collect::<Vec<_>>();
        let rolling_usage = self
            .accounts
            .load_account_usage(rolling_range, &ids)
            .await
            .map_err(|error| map_store_error(error, "rolling account usage"))?;
        let rolling_usage = rolling_usage
            .into_iter()
            .map(|usage| (usage.account_id.clone(), usage))
            .collect::<BTreeMap<_, _>>();
        let mut quotas = futures::future::join_all(page.items.iter().map(|item| async {
            let account = &item.account;
            let account_id = ProviderAccountId::new(account.id.clone())
                .map_err(|_| AdminError::invalid("Provider 账号 ID 不合法"))?;
            // 单个账号的 quota 投影失败（Provider 未注册或 quota 读取失败）不拖垮整页：
            // 该账号降级为空额度投影，其余账号与页面状态照常返回。
            let provider = match self.providers.require(&account.provider_kind) {
                Ok(provider) => provider,
                Err(error) => {
                    tracing::warn!(
                        account_id = %account.id,
                        error = %error,
                        "account directory provider is not registered; showing empty quota"
                    );
                    return Ok(empty_quota());
                }
            };
            match provider
                .quota(ProviderQuotaRequest {
                    account_id,
                    refresh: false,
                    rolling_usage: rolling_usage.get(&account.id).cloned(),
                })
                .await
            {
                Ok(quota) => Ok(quota),
                Err(error) => {
                    tracing::warn!(
                        account_id = %account.id,
                        error = %error,
                        "account directory quota projection failed; showing empty quota"
                    );
                    Ok(empty_quota())
                }
            }
        }))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, AdminError>>()?;
        self.attach_quota_local_usage(&page.items, &mut quotas)
            .await?;
        let mut api_key_usage = self.load_api_key_usage(&page.items).await?;
        let items = page
            .items
            .into_iter()
            .zip(quotas)
            .map(|(mut item, quota)| {
                let usage = api_key_usage.remove(&item.account.id).or_else(|| {
                    quota
                        .usage_window()
                        .and_then(|(window, _)| window.local_usage.clone())
                });
                AccountDirectoryItem {
                    plan_type_display: self.providers.resolve_account_plan(
                        item.account.provider_kind.as_str(),
                        &mut item.account.plan_type,
                        Some(&quota),
                    ),
                    usage,
                    account: item.account,
                    projection: item.projection,
                    quota,
                }
            })
            .collect();
        Ok(AccountDirectoryPage {
            config_revision: page.config_revision,
            items,
            total: page.total,
            summary: page.summary,
        })
    }

    async fn export(
        &self,
        context: &MutationContext,
        account_ids: Vec<ProviderAccountId>,
    ) -> Result<AccountExportBundle, AdminError> {
        if account_ids.is_empty() || account_ids.len() > 200 {
            return Err(AdminError::invalid("账号导出数量必须在 1 到 200 之间"));
        }
        let exported_ids = account_ids.clone();
        let mut grouped = BTreeMap::<ProviderKind, Vec<ProviderAccountId>>::new();
        for account_id in account_ids {
            let account = self.load_account(&account_id).await?;
            grouped
                .entry(account.account.provider_kind)
                .or_default()
                .push(account_id);
        }
        if grouped.values().any(|ids| {
            let unique = ids.iter().collect::<std::collections::BTreeSet<_>>();
            unique.len() != ids.len()
        }) {
            return Err(AdminError::invalid("账号导出列表包含重复 ID"));
        }
        let mut documents = Vec::with_capacity(grouped.len());
        for (provider_kind, ids) in grouped {
            let provider = self
                .providers
                .require(&provider_kind)
                .map_err(|error| map_provider_error(error, "provider account export"))?;
            let credentials = self
                .accounts
                .load_credentials_for_export(&provider_kind, &ids)
                .await
                .map_err(|error| map_store_error(error, "provider account export"))?;
            documents.push(
                provider
                    .export_credentials(credentials)
                    .await
                    .map_err(|error| map_provider_error(error, "provider account export"))?,
            );
        }
        self.accounts
            .record_credential_export(&exported_ids, context)
            .await
            .map_err(|error| map_store_error(error, "provider account export audit"))?;
        Ok(AccountExportBundle {
            exported_at: Utc::now(),
            documents,
        })
    }

    async fn refresh(
        &self,
        context: &MutationContext,
        account_id: ProviderAccountId,
    ) -> Result<AccountRefreshResult, AdminError> {
        let (stored, provider) = self.provider_for_account(&account_id).await?;
        let account = stored.account;
        let prepared = provider
            .prepare_refresh(PrepareCredentialRefresh {
                account: account.clone(),
            })
            .await
            .map_err(|error| map_provider_error(error, "provider credential refresh"))?;
        validate_prepared_rotation(&account, &prepared, "provider credential refresh")?;
        let result = commit_credential_refresh(
            self.accounts.as_ref(),
            prepared,
            context,
            "provider credential refresh",
        )
        .await?;
        provider
            .account_facts_changed(std::slice::from_ref(&result.account_id))
            .await;
        publish_committed(self.snapshot.as_ref(), result.config_revision).await?;
        let account = self.load_directory_item(&result.account_id, false).await?;
        Ok(AccountRefreshResult {
            config_revision: result.config_revision,
            account,
        })
    }

    async fn recover(
        &self,
        context: &MutationContext,
        account_id: ProviderAccountId,
    ) -> Result<AccountRefreshResult, AdminError> {
        let (stored, provider) = self.provider_for_account(&account_id).await?;
        let config_revision = if stored.account.enabled {
            self.accounts
                .recover_account(&account_id, context)
                .await
                .map_err(|error| map_store_error(error, "provider account recovery"))?
                .config_revision
        } else {
            // 停用只表示不参与调度，重新启用不能抹除已观测的额度、凭据或冷却事实。
            self.accounts
                .batch_update_accounts(
                    BatchUpdateAccounts {
                        account_ids: vec![account_id.to_string()],
                        enabled: Some(true),
                        concurrency_limit: None,
                        weight: None,
                        model_access: None,
                        group_ids: None,
                        outbound_proxy: None,
                    },
                    context,
                )
                .await
                .map_err(|error| map_store_error(error, "enable provider account"))?
                .config_revision
        };
        provider
            .account_facts_changed(std::slice::from_ref(&account_id))
            .await;
        publish_committed(self.snapshot.as_ref(), config_revision).await?;
        let account = self.load_directory_item(&account_id, false).await?;
        Ok(AccountRefreshResult {
            config_revision,
            account,
        })
    }

    async fn update(
        &self,
        context: &MutationContext,
        command: UpdateAccount,
    ) -> Result<AccountUpdateResult, AdminError> {
        let account_id = ProviderAccountId::new(command.account_id.clone())
            .map_err(|_| AdminError::invalid("Provider 账号 ID 不合法"))?;
        let (_, provider) = self.provider_for_account(&account_id).await?;
        let enabled = command.enabled;
        let result = self
            .accounts
            .update_account(command, context)
            .await
            .map_err(|error| map_store_error(error, "provider account"))?;
        if !enabled {
            provider.account_unavailable(&account_id).await;
        }
        provider
            .account_facts_changed(std::slice::from_ref(&account_id))
            .await;
        publish_committed(self.snapshot.as_ref(), result.config_revision).await?;
        Ok(result)
    }

    async fn lower_concurrency_limit(
        &self,
        context: &MutationContext,
        account_id: ProviderAccountId,
        limit: gateway_core::account::AccountConcurrencyLimit,
    ) -> Result<Option<AccountUpdateResult>, AdminError> {
        let (_, provider) = self.provider_for_account(&account_id).await?;
        let result = self
            .accounts
            .lower_concurrency_limit(&account_id, limit, context)
            .await
            .map_err(|error| map_store_error(error, "provider account"))?;
        if let Some(result) = &result {
            provider
                .account_facts_changed(std::slice::from_ref(&account_id))
                .await;
            publish_committed(self.snapshot.as_ref(), result.config_revision).await?;
        }
        Ok(result)
    }

    async fn batch_update(
        &self,
        context: &MutationContext,
        command: BatchUpdateAccounts,
    ) -> Result<AccountsUpdateResult, AdminError> {
        let account_ids = command
            .account_ids
            .iter()
            .map(|id| {
                ProviderAccountId::new(id.clone())
                    .map_err(|_| AdminError::invalid("Provider 账号 ID 不合法"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut providers = BTreeMap::<
            ProviderKind,
            (
                Arc<dyn crate::ports::provider::ProviderAdmin>,
                Vec<ProviderAccountId>,
            ),
        >::new();
        for account_id in &account_ids {
            let (item, provider) = self.provider_for_account(account_id).await?;
            providers
                .entry(item.account.provider_kind)
                .or_insert_with(|| (provider, Vec::new()))
                .1
                .push(account_id.clone());
        }
        let enabled = command.enabled;
        let result = self
            .accounts
            .batch_update_accounts(command, context)
            .await
            .map_err(|error| map_store_error(error, "provider accounts"))?;
        for (provider, provider_ids) in providers.values() {
            if enabled == Some(false) {
                for account_id in provider_ids {
                    provider.account_unavailable(account_id).await;
                }
            }
            provider.account_facts_changed(provider_ids).await;
        }
        publish_committed(self.snapshot.as_ref(), result.config_revision).await?;
        Ok(result)
    }

    async fn account_configuration(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<Option<crate::model::provider_credentials::ProviderDocument>, AdminError> {
        let (_, provider) = self.provider_for_account(account_id).await?;
        provider
            .account_configuration(account_id)
            .await
            .map_err(|error| map_provider_error(error, "provider account configuration"))
    }

    async fn quota(
        &self,
        account_id: &ProviderAccountId,
        refresh: bool,
    ) -> Result<AccountDirectoryItem, AdminError> {
        self.load_directory_item(account_id, refresh).await
    }

    async fn quota_forecast(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<AccountQuotaForecastReport, AdminError> {
        let (stored, provider) = self.provider_for_account(account_id).await?;
        let quota = provider
            .quota(ProviderQuotaRequest {
                account_id: account_id.clone(),
                refresh: false,
                rolling_usage: None,
            })
            .await
            .map_err(|error| map_provider_error(error, "forecast quota snapshot"))?;
        let now = Utc::now();
        let mut samples = Vec::new();
        for (window, _) in quota.usage_windows() {
            let (Some(mut query), Some(observed), Some(percent)) = (
                quota_usage_window(account_id.as_str(), window),
                quota.observed_at,
                window.used_percent,
            ) else {
                continue;
            };
            query.range.start = query.range.start.max(stored.account.created_at);
            if query.range.start >= observed
                || observed > now
                || now >= query.range.end
                || !percent.is_finite()
                || !(0.0..=100.0).contains(&percent)
            {
                continue;
            }
            let reset_at = query.range.end;
            query.range.end = observed;
            let history = self
                .accounts
                .load_quota_forecast_history(&query)
                .await
                .map_err(|error| map_store_error(error, "forecast paired usage"))?;
            let mut points = Vec::new();
            let mut interrupted = false;
            for point in history.points {
                let Some(fact) =
                    provider.quota_forecast_observation(&point.provider_observation, window)
                else {
                    continue;
                };
                let same_plan = quota
                    .plan_type
                    .as_deref()
                    .zip(fact.plan_type.as_deref())
                    .is_some_and(|(current, previous)| current.eq_ignore_ascii_case(previous));
                // 仅容纳已观测到的秒级量化抖动，不用宽时间容差合并实际重置。
                // 不匹配的段截断基线；之后的有效观测可以重新积累。
                if !same_plan || (fact.reset_at - reset_at).abs() > Duration::seconds(2) {
                    points.clear();
                    interrupted = true;
                    continue;
                }
                points.push(QuotaForecastPoint {
                    observed_at: point.completed_at,
                    used_percent: fact.used_percent,
                    usage: point.usage,
                });
            }
            let sample = select_forecast_sample(
                window.key.clone(),
                query.range.start,
                QuotaForecastPoint {
                    observed_at: observed,
                    used_percent: percent,
                    usage: history.usage,
                },
                points,
                history.pending_request_count,
                interrupted,
            );
            samples.push(sample);
        }
        Ok(AccountQuotaForecastReport {
            account_id: account_id.to_string(),
            generated_at: now,
            forecasts: account_quota_forecasts(&quota, stored.account.created_at, now, &samples),
        })
    }

    async fn personal_info(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<AccountPersonalInfo, AdminError> {
        let (initial, provider) = self.provider_for_account(account_id).await?;
        // 两项读取相互独立；不因其中一项失败而取消另一项，也不触发凭据或额度刷新。
        let (profile, subscription) = futures::join!(
            provider.profile_statistics(account_id),
            provider.subscription(account_id),
        );
        let profile =
            profile.map_err(|error| map_provider_error(error, "provider profile statistics"));
        let subscription = subscription
            .map_err(|error| map_provider_error(error, "provider subscription"))
            .ok()
            .flatten();

        // 汇聚等待期间发生重新授权、换绑或删除时，不返回混合身份的数据。
        let current = self.load_account(account_id).await?;
        if current.account.provider_kind != initial.account.provider_kind
            || current.account.credential_revision != initial.account.credential_revision
            || current.account.upstream_user_id != initial.account.upstream_user_id
            || current.account.upstream_account_id != initial.account.upstream_account_id
        {
            return Err(AdminError::conflict("账号身份已变化，请刷新信息后重试"));
        }
        Ok(AccountPersonalInfo {
            profile,
            subscription,
        })
    }

    async fn profile_avatar(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderProfileAvatar, AdminError> {
        let (_, provider) = self.provider_for_account(account_id).await?;
        provider
            .profile_avatar(account_id)
            .await
            .map_err(|error| map_provider_error(error, "provider profile avatar"))
    }

    async fn reset_credits(
        &self,
        context: &MutationContext,
        account_id: ProviderAccountId,
    ) -> Result<ProviderResetCredits, AdminError> {
        let (_, provider) = self.provider_for_account(&account_id).await?;
        match provider.reset_credits(&account_id).await {
            Ok(credits) => Ok(credits),
            Err(error)
                if error.kind()
                    == crate::ports::provider::ProviderAdminErrorKind::CredentialRefreshRequired =>
            {
                self.refresh(context, account_id.clone()).await?;
                let (_, provider) = self.provider_for_account(&account_id).await?;
                provider
                    .reset_credits(&account_id)
                    .await
                    .map_err(map_reset_credits_error_after_refresh)
            }
            Err(error) => Err(map_provider_error(error, "provider reset credits")),
        }
    }

    async fn consume_reset_credit(
        &self,
        context: &MutationContext,
        command: ConsumeProviderResetCredit,
    ) -> Result<ProviderResetCreditResult, AdminError> {
        let account_id = command.account_id.clone();
        // 覆盖 credential refresh + 同键重试的完整账号级临界区，避免 401 两次调用
        // 之间插入另一笔不可逆消费。
        let lock = self.reset_credit_lock(&account_id).await;
        let _guard = lock.lock().await;
        let (_, provider) = self.provider_for_account(&account_id).await?;
        match provider.consume_reset_credit(command.clone()).await {
            Ok(result) => Ok(result),
            Err(error)
                if error.kind()
                    == crate::ports::provider::ProviderAdminErrorKind::CredentialRefreshRequired =>
            {
                self.refresh(context, account_id.clone()).await?;
                let (_, provider) = self.provider_for_account(&account_id).await?;
                provider
                    .consume_reset_credit(command)
                    .await
                    .map_err(map_reset_credits_error_after_refresh)
            }
            Err(error) => Err(map_provider_error(error, "provider reset-credit consume")),
        }
    }

    async fn models(
        &self,
        account_id: &ProviderAccountId,
        refresh: bool,
    ) -> Result<ProviderModels, AdminError> {
        let (_, provider) = self.provider_for_account(account_id).await?;
        provider
            .models(account_id, refresh)
            .await
            .map_err(|error| map_provider_error(error, "provider model catalog"))
    }

    async fn test_connection(
        &self,
        account_id: ProviderAccountId,
        upstream_model: UpstreamModelId,
    ) -> Result<AccountConnectionTestEventStream, AdminError> {
        let (stored, provider) = self.provider_for_account(&account_id).await?;
        let account = stored.account;
        let model = upstream_model.as_str().to_owned();
        let operation = provider
            .connection_test_operation(&upstream_model, CONNECTION_TEST_INPUT)
            .map_err(|error| map_provider_error(error, "provider connection test"))?;
        let initial = vec![
            AccountConnectionTestEvent::Started {
                model: model.clone(),
            },
            AccountConnectionTestEvent::Request {
                model,
                input_text: CONNECTION_TEST_INPUT.to_owned(),
                stream: true,
                store: false,
            },
        ];
        let probe = Arc::clone(&self.probe);
        let terminal = futures::stream::once(async move {
            let result = probe
                .probe(AccountProbeRequest {
                    account_id,
                    provider_kind: account.provider_kind,
                    upstream_model,
                    operation,
                })
                .await;
            match result {
                Ok(result) => result
                    .text
                    .into_iter()
                    .map(|text| AccountConnectionTestEvent::Content { text })
                    .chain(std::iter::once(AccountConnectionTestEvent::Completed))
                    .collect(),
                Err(error) => {
                    let upstream_status = error
                        .upstream_response()
                        .map(gateway_core::engine::probe::AccountProbeUpstreamResponse::status);
                    let upstream_content_type = error
                        .upstream_response()
                        .and_then(|response| response.content_type())
                        .and_then(|value| std::str::from_utf8(value).ok())
                        .map(ToOwned::to_owned);
                    let upstream_body = error
                        .upstream_response()
                        .map(|response| String::from_utf8_lossy(response.body()).into_owned());
                    let message = error.client_message().to_owned();
                    vec![AccountConnectionTestEvent::Failed {
                        source: error.source(),
                        gateway_error_code: error.kind(),
                        send_state: error.send_state(),
                        message,
                        provider_error_code: error.client_error_code().map(ToOwned::to_owned),
                        provider_error_type: error.client_error_type().map(ToOwned::to_owned),
                        upstream_status,
                        upstream_content_type,
                        upstream_body,
                    }]
                }
            }
        })
        .flat_map(futures::stream::iter);
        Ok(Box::pin(futures::stream::iter(initial).chain(terminal)))
    }

    async fn turn_state_snapshot(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
    ) -> Result<Option<TurnStateSnapshot>, AdminError> {
        let (_, provider) = self.provider_for_account(account_id).await?;
        Ok(provider.turn_state_snapshot(account_id, model))
    }

    async fn turn_state_overview(&self) -> Result<Vec<TurnStateOverviewEntry>, AdminError> {
        Ok(self.providers.turn_state_overview())
    }

    async fn test_turn_state_policy(
        &self,
        account_id: ProviderAccountId,
        model: UpstreamModelId,
        policy: crate::model::turn_state::TurnStateProbePolicy,
    ) -> Result<TurnStateProbeResult, AdminError> {
        policy.validate()?;
        let (_, provider) = self.provider_for_account(&account_id).await?;
        let targets = self.turn_state_probe_targets().await?;
        provider
            .test_turn_state_policy(&account_id, &model, targets, policy)
            .await
            .map_err(|error| map_provider_error(error, "Codex turn state draft"))
    }

    async fn probe_turn_state(
        &self,
        account_id: ProviderAccountId,
        model: UpstreamModelId,
    ) -> Result<TurnStateProbeResult, AdminError> {
        self.probe_turn_state_with_source(account_id, model, TurnStateSource::ManualProbe)
            .await
    }

    async fn turn_state_capture_status(&self) -> Result<TurnStateCaptureStatus, AdminError> {
        Ok(self.turn_state_probe_capture.status())
    }

    async fn start_turn_state_capture(
        &self,
        context: &MutationContext,
        duration: std::time::Duration,
    ) -> Result<TurnStateCaptureStatus, AdminError> {
        if !matches!(duration.as_secs(), 900 | 3_600 | 21_600) {
            return Err(AdminError::invalid(
                "采集时长仅支持 15 分钟、1 小时或 6 小时",
            ));
        }
        self.append_probe_audit(
            context,
            "turn_state_probe_capture.start",
            "turn_state_probe_capture",
            "runtime",
            vec!["enabled_until".to_owned()],
        )
        .await?;
        Ok(self.turn_state_probe_capture.start(duration))
    }

    async fn stop_turn_state_capture(
        &self,
        context: &MutationContext,
    ) -> Result<TurnStateCaptureStatus, AdminError> {
        // 停止敏感采集优先于审计可用性，避免审计故障使采集继续运行。
        let status = self.turn_state_probe_capture.stop();
        self.append_probe_audit(
            context,
            "turn_state_probe_capture.stop",
            "turn_state_probe_capture",
            "runtime",
            vec!["enabled_until".to_owned()],
        )
        .await?;
        Ok(status)
    }

    async fn list_turn_state_probe_exchanges(
        &self,
        query: TurnStateProbeExchangeQuery,
    ) -> Result<TurnStateProbeExchangePage, AdminError> {
        self.turn_state_probe_capture
            .list(query)
            .await
            .map_err(|error| map_store_error(error, "turn state probe exchanges"))
    }

    async fn turn_state_probe_exchange_detail(
        &self,
        id: &str,
    ) -> Result<TurnStateProbeExchangeDetail, AdminError> {
        self.turn_state_probe_capture
            .detail(id)
            .await
            .map_err(|error| map_store_error(error, "turn state probe exchange"))?
            .ok_or_else(|| AdminError::not_found("探测报头记录不存在或已过期"))
    }

    async fn reveal_turn_state_probe_exchange(
        &self,
        context: &MutationContext,
        id: &str,
    ) -> Result<TurnStateProbeExchangeDetail, AdminError> {
        let detail = self.turn_state_probe_exchange_detail(id).await?;
        self.append_probe_audit(
            context,
            "turn_state_probe_capture.reveal",
            "turn_state_probe_capture",
            id,
            vec!["request_headers".to_owned(), "response_headers".to_owned()],
        )
        .await?;
        Ok(detail)
    }
}

pub(crate) struct TurnStateRenewalTask {
    accounts: Arc<DefaultAccountsService>,
}

impl TurnStateRenewalTask {
    #[must_use]
    pub(crate) fn new(accounts: Arc<DefaultAccountsService>) -> Self {
        Self { accounts }
    }
}

impl ScheduledTask for TurnStateRenewalTask {
    fn run_cycle(&self, context: WorkerCycleContext) -> BoxFuture<'_, Result<(), WorkerTaskError>> {
        Box::pin(async move {
            tokio::select! {
                () = context.cancellation().cancelled() => {},
                () = self.accounts.renew_due_turn_states() => {},
            }
            Ok(())
        })
    }
}

fn map_reset_credits_error_after_refresh(
    error: crate::ports::provider::ProviderAdminError,
) -> AdminError {
    if error.kind() == crate::ports::provider::ProviderAdminErrorKind::CredentialRefreshRequired {
        return AdminError::bad_gateway("上游服务拒绝了刷新后的凭据");
    }
    map_provider_error(error, "provider reset credits")
}

/// 账号目录中单个账号 quota 读取失败时使用的空额度投影。
fn empty_quota() -> ProviderQuota {
    ProviderQuota {
        plan_type: None,
        observed_at: None,
        refresh_token_expires_at: None,
        windows: Vec::new(),
        limit_reached: false,
        provider_data: None,
    }
}

fn quota_usage_window(
    account_id: &str,
    window: &ProviderQuotaWindow,
) -> Option<AccountUsageWindowQuery> {
    if window.local_usage_attribution != QuotaLocalUsageAttribution::AccountWide {
        return None;
    }
    let reset_at = window.reset_at?;
    let seconds = i64::try_from(window.window_seconds?).ok()?;
    let start = reset_at.checked_sub_signed(Duration::try_seconds(seconds)?)?;
    let range = TimeRange::new(start, reset_at).ok()?;
    // 上游百分比以该 reset 边界定义；以当前时间回推会让本地 Token 属于另一窗口。
    Some(AccountUsageWindowQuery {
        account_id: account_id.to_owned(),
        key: window.key.clone(),
        range,
    })
}
