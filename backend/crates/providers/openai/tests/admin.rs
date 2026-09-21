use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeZone as _, Utc};
use futures::{StreamExt, future::BoxFuture};
use gateway_admin::model::accounts::AccountRecord;
use gateway_admin::model::observability::{
    CurrencyCost, DesktopReleaseStatus, ProviderBillingInput,
};
use gateway_admin::model::provider_credentials::{
    AuthorizationMutationTarget, AuthorizationOwnerBinding, CompleteAuthorization,
    ConsumeProviderResetCredit, PendingAuthorizationMutation, PrepareCredentialImport,
    PrepareCredentialRefresh, PrepareCredentialRotation, ProviderDocument,
    ProviderExportCredentialInput, ProviderQuotaRequest, ProviderQuotaWindowRole,
    QuotaLocalUsageAttribution,
};
use gateway_admin::model::turn_state::{TurnStateProbeTarget, TurnStateSource};
use gateway_admin::model::turn_state_capture::TurnStateProbeExchangeCapture;
use gateway_admin::model::{MutationActor, MutationContext, Revision};
use gateway_admin::ports::provider::ProviderAdminErrorKind;
use gateway_admin::ports::turn_state_capture::TurnStateProbeCaptureSink;
use gateway_core::account::{
    CredentialRevision, CredentialState, OpaqueProviderData, OutboundProxy, ProviderAccount,
    ProviderAccountId, ProviderAccountStore, QuotaAccessChange, QuotaEvidence, QuotaObservation,
    QuotaState,
};
use gateway_core::engine::provider::ProviderRequest;
use gateway_core::engine::{
    AccountAttemptContext, AttemptContext, ModelRequestId, RequestAttemptContext,
};
use gateway_core::lifecycle::CancellationToken;
use gateway_core::operation::{GenerateRequest, Operation, ProtocolPayload};
use gateway_core::policy::ClientApiKeyId;
use gateway_core::provider_ports::{
    NewOAuthPendingFlow, OAuthPendingClaimOutcome, OAuthPendingConsumeOutcome,
    OAuthPendingFlowPort, OAuthPendingPutOutcome, OAuthPendingReleaseOutcome,
    ProviderArtifactProfile, ProviderArtifactProfileCachePort, ProviderCatalogCacheKey,
    ProviderCatalogCachePort, ProviderCooldown, ProviderCooldownPort, ProviderCooldownScope,
    ProviderCredentialState, ProviderCredentialStatePort, ProviderRefreshPolicy,
    ProviderRuntimePolicyPort, ProviderScopedCooldown, ProviderStoreError, ProviderStorePorts,
};
use gateway_core::routing::{
    ClientRoutingScope, ConfigRevision, FrozenAccountScope, ModelCapabilities, ProviderKind,
    ProviderModel, PublicModelId, RoutingContext, RuntimeAccount, RuntimeAccountDirectory,
    RuntimeSnapshot, UpstreamModelId,
};
use gateway_core::task::{WorkerContribution, WorkerKind, WorkerRunnable};
use provider_openai::config::OpenAiConfig;
use provider_openai::credential::{CodexCredentialCodec, ImportCodexOAuthCredential};
use provider_openai::transport::profile::APPCAST_POLL_INTERVAL;
use secrecy::SecretString;
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tokio::sync::Notify;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{
    MemoryAccountStore, MemorySessionAffinity, MemorySessionExclusions, TestLeaseCoordinator,
    account_policy, profile, secret,
};

const COMPLETED_SESSION_SSE: &str = concat!(
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_initialized_session\",\"model\":\"gpt-5.4\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
);

#[derive(Default)]
struct TestTurnStateProbeCapture {
    captures: Mutex<Vec<TurnStateProbeExchangeCapture>>,
}

impl TestTurnStateProbeCapture {
    fn take(&self) -> Vec<TurnStateProbeExchangeCapture> {
        std::mem::take(&mut *self.captures.lock().expect("capture lock"))
    }
}

impl TurnStateProbeCaptureSink for TestTurnStateProbeCapture {
    fn enabled(&self) -> bool {
        true
    }

    fn try_capture(&self, capture: TurnStateProbeExchangeCapture) -> bool {
        self.captures.lock().expect("capture lock").push(capture);
        true
    }
}

#[tokio::test]
async fn openai_bundle_exposes_one_core_provider_and_drains_worker_contributions_once() {
    let config = valid_config();
    let mut bundle = provider_openai::initialize(config.config.clone(), provider_ports())
        .await
        .expect("OpenAI bundle");

    assert_eq!(bundle.core_provider().name(), "openai");
    assert_eq!(bundle.admin_provider().provider_kind().as_str(), "openai");
    let contributions = bundle.take_worker_contributions();
    assert_eq!(contributions.len(), 7);
    assert!(
        contributions
            .iter()
            .any(|item| item.kind() == WorkerKind::OAuthRefresh)
    );
    assert!(
        contributions
            .iter()
            .any(|item| item.kind() == WorkerKind::QuotaCatalogHealth)
    );
    let release_worker = contributions
        .iter()
        .find_map(|contribution| match contribution {
            WorkerContribution::Registration(registration)
                if registration.id.owner() == "openai-desktop-release" =>
            {
                Some(registration)
            }
            WorkerContribution::Registration(_) | WorkerContribution::Disabled { .. } => None,
        })
        .expect("Desktop release worker");
    assert_eq!(release_worker.id.kind(), WorkerKind::QuotaCatalogHealth);
    let WorkerRunnable::Scheduled { schedule, .. } = &release_worker.runnable else {
        panic!("Desktop release worker must be scheduled");
    };
    assert_eq!(schedule.interval(), APPCAST_POLL_INTERVAL);
    for (owner, interval) in [
        ("openai-cli-release", APPCAST_POLL_INTERVAL),
        ("openai-platform-desktop-release", APPCAST_POLL_INTERVAL),
        ("openai", Duration::from_secs(30)),
        (
            "openai-model-catalog",
            config.config.quota_refresh_policy().interval(),
        ),
    ] {
        let schedule = contributions
            .iter()
            .find_map(|contribution| match contribution {
                WorkerContribution::Registration(registration)
                    if registration.id.kind() == WorkerKind::QuotaCatalogHealth
                        && registration.id.owner() == owner =>
                {
                    match &registration.runnable {
                        WorkerRunnable::Scheduled { schedule, .. } => Some(schedule),
                        WorkerRunnable::Daemon { .. } => None,
                    }
                }
                _ => None,
            })
            .expect("quota/catalog scheduled worker");
        assert_eq!(schedule.interval(), interval, "{owner}");
    }
    assert!(contributions.iter().any(|contribution| {
        matches!(
            contribution,
            WorkerContribution::Registration(registration)
                if registration.id.owner() == "openai-model-etag"
                    && matches!(&registration.runnable, WorkerRunnable::Daemon { .. })
        )
    }));
    assert!(bundle.take_worker_contributions().is_empty());
}

#[tokio::test]
async fn quota_forecast_observation_reuses_protocol_parser_and_keeps_window_identity() {
    let config = valid_config();
    let bundle = provider_openai::initialize(config.config.clone(), provider_ports())
        .await
        .unwrap();
    let provider = bundle.admin_provider();
    let mut window = gateway_admin::model::provider_credentials::ProviderQuotaWindow {
        key: "codex:604800s".to_owned(),
        group: "shortTerm".to_owned(),
        label: "周额度".to_owned(),
        limit_id: Some("codex".to_owned()),
        limit_name: None,
        role: Some(ProviderQuotaWindowRole::Primary),
        local_usage_attribution: QuotaLocalUsageAttribution::AccountWide,
        window_seconds: Some(604_800),
        used_percent: Some(50.0),
        reset_at: None,
        limit_reached: false,
        local_usage: None,
        provider_data: None,
    };
    let document = ProviderDocument::new(OpaqueProviderData::new(
        json!({
            "requestSummary": {"ignored": true},
            "rateLimitHeaders": [
                ["x-codex-primary-used-percent", "32.5"],
                ["x-codex-primary-window-minutes", "10080"],
                ["x-codex-primary-reset-at", "1789805447"],
                ["x-codex-secondary-used-percent", "4"],
                ["x-codex-secondary-window-minutes", "300"],
                ["x-codex-secondary-reset-at", "1789218647"],
                ["x-codex-plan-type", "pro"]
            ]
        })
        .as_object()
        .unwrap()
        .clone(),
    ));
    let observed = provider
        .quota_forecast_observation(&document, &window)
        .unwrap();
    assert_eq!(observed.used_percent, 32.5);
    assert_eq!(observed.plan_type.as_deref(), Some("pro"));
    assert_eq!(observed.reset_at.timestamp(), 1_789_805_447);
    window.role = Some(ProviderQuotaWindowRole::Secondary);
    assert!(
        provider
            .quota_forecast_observation(&document, &window)
            .is_none()
    );
    window.role = Some(ProviderQuotaWindowRole::Primary);
    window.limit_id = Some("other_bucket".to_owned());
    assert!(
        provider
            .quota_forecast_observation(&document, &window)
            .is_none()
    );
    let malformed = ProviderDocument::new(OpaqueProviderData::new(
        json!({
            "rateLimitHeaders": "not-a-header-list"
        })
        .as_object()
        .unwrap()
        .clone(),
    ));
    assert!(
        provider
            .quota_forecast_observation(&malformed, &window)
            .is_none()
    );
}

#[tokio::test]
async fn initialized_provider_keeps_thread_spawn_transport_conversations_distinct() {
    let account_id = "acct_initialized_thread_spawn";
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: account_id.to_owned(),
            name: account_id.to_owned(),
            secret: secret("at-initialized-thread-spawn"),
            verified_account: profile("chatgpt-initialized-thread-spawn"),
            next_refresh_at: Some(Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(COMPLETED_SESSION_SSE),
        )
        .expect(2)
        .mount(&server)
        .await;
    let mut config = valid_config();
    config.config.api.base_url = server.uri();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(store, Arc::new(TestOAuthPending::default())),
    )
    .await
    .expect("initialized OpenAI provider");
    let provider = bundle.core_provider();
    let thread_spawn = r#"{"subagent_kind":"thread_spawn"}"#;
    let mut conversation_ids = Vec::new();

    for (request_id, thread_id) in [
        ("req_initialized_thread_spawn_first", "child-one"),
        ("req_initialized_thread_spawn_second", "child-two"),
    ] {
        let payload = ProtocolPayload::json_object(
            "openai",
            Map::from_iter([
                ("model".to_owned(), json!("gpt-5.4")),
                ("input".to_owned(), json!("child task")),
                ("session_id".to_owned(), json!("parent-session")),
                ("thread_id".to_owned(), json!(thread_id)),
                ("turnMetadata".to_owned(), json!(thread_spawn)),
            ]),
        )
        .expect("OpenAI payload")
        .with_context(Map::from_iter([("use_websocket".to_owned(), json!(false))]));
        let operation = Operation::Generate(GenerateRequest::from_protocol_payload(payload));
        let mut stream = provider
            .execute(
                initialized_provider_request(operation, account_id),
                initialized_attempt_context(request_id, account_id),
            )
            .await
            .expect("prepare child provider stream");
        let mut conversation_id = None;
        while let Some(event) = stream.next().await {
            let event = event.expect("child provider response");
            if let Some(update) = event.session_update() {
                conversation_id = update
                    .payload()
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
        }
        conversation_ids.push(conversation_id.expect("child transport conversation id"));
    }

    assert_ne!(conversation_ids[0], conversation_ids[1]);
}

#[tokio::test]
async fn copying_builtin_prices_keeps_cache_read_and_write_fallback_costs() {
    use provider_openai::transport::{
        OpenAiBillingUsage, openai_billing_breakdown, openai_billing_breakdown_with_override,
    };
    let config = valid_config();
    let bundle = provider_openai::initialize(config.config.clone(), provider_ports())
        .await
        .unwrap();
    let prices = bundle.admin_provider().pricing_catalog();
    for model in ["gpt-4", "gpt-4o", "gpt-6-astra"] {
        let usage = OpenAiBillingUsage::new(100, 10, 20, 15);
        let inherited = openai_billing_breakdown(model, usage, None).unwrap();
        let copied =
            openai_billing_breakdown_with_override(model, usage, None, Some(&prices[model]))
                .unwrap();
        assert_eq!(inherited.total_amount(), copied.total_amount(), "{model}");
    }
}

#[tokio::test]
async fn openai_admin_provider_exposes_live_wire_profile_and_validated_billing() {
    let config = valid_config();
    let bundle = provider_openai::initialize(config.config.clone(), provider_ports())
        .await
        .expect("OpenAI bundle");
    let admin = bundle.admin_provider();
    let baseline = admin.dashboard_wire_profile().expect("official baseline");
    assert_eq!(
        baseline.release.as_ref().map(|release| release.status),
        Some(DesktopReleaseStatus::Unchecked)
    );
    let selection = OpaqueProviderData::new(json!({
        "client": "desktop", "platform": "macos", "versionMode": "fixed",
        "codexVersion": "0.102.0", "desktopVersion": "1.2026.190", "desktopBuild": "19012345678",
        "osVersion": "15.5.0", "arch": "arm64", "terminal": "xterm-256color"
    }).as_object().unwrap().clone());
    let profile = admin
        .configured_wire_profile(&selection)
        .expect("managed fixed profile");
    assert_eq!(profile.version, "0.102.0");
    assert_eq!(profile.build, None);
    assert_eq!(profile.target.os_type, "Mac OS");
    assert_eq!(profile.target.os_version, "15.5.0");
    assert_eq!(
        profile.user_agent,
        "Codex Desktop/0.102.0 (Mac OS 15.5.0; arm64) xterm-256color (Codex Desktop; 1.2026.190)"
    );
    assert_eq!(
        profile
            .attributes
            .iter()
            .find(|attribute| attribute.label == "客户端标识")
            .map(|attribute| attribute.value.as_str()),
        Some("Codex Desktop; 1.2026.190")
    );
    assert!(profile.release.is_none());
    let billing = admin
        .calculated_billing(&ProviderBillingInput {
            upstream_model_id: "gpt-4o".to_owned(),
            service_tier: None,
            input_tokens: Some(1_000_000),
            output_tokens: Some(0),
            cached_tokens: Some(0),
            cache_write_tokens: Some(0),
            total: CurrencyCost {
                currency: "USD".to_owned(),
                amount: "2.5".parse().expect("amount"),
            },
        })
        .expect("billing")
        .expect("known pricing");
    assert_eq!(billing.total_amount.amount.as_str(), "2.5");
    assert_eq!(billing.input_price_per_million.amount.as_str(), "2.5");

    let fast_billing = admin
        .calculated_billing(&ProviderBillingInput {
            upstream_model_id: "gpt-4o".to_owned(),
            service_tier: Some("priority".to_owned()),
            input_tokens: Some(1_000_000),
            output_tokens: Some(0),
            cached_tokens: Some(0),
            cache_write_tokens: Some(0),
            total: CurrencyCost {
                currency: "USD".to_owned(),
                amount: "4.25".parse().expect("fast amount"),
            },
        })
        .expect("fast billing")
        .expect("known fast pricing");
    assert_eq!(fast_billing.service_tier.as_deref(), Some("priority"));
    assert_eq!(fast_billing.multiplier_percent, 170);
    assert_eq!(fast_billing.standard_amount.amount.as_str(), "2.5");
    assert_eq!(fast_billing.total_amount.amount.as_str(), "4.25");
}

#[tokio::test]
async fn openai_legacy_billing_should_not_infer_long_context_flag() {
    let config = valid_config();
    let bundle = provider_openai::initialize(config.config.clone(), provider_ports())
        .await
        .expect("OpenAI bundle");
    let billing = bundle
        .admin_provider()
        .calculated_billing(&ProviderBillingInput {
            upstream_model_id: "gpt-5.4".to_owned(),
            service_tier: None,
            input_tokens: Some(300_000),
            output_tokens: Some(0),
            cached_tokens: Some(0),
            cache_write_tokens: Some(0),
            total: CurrencyCost {
                currency: "USD".to_owned(),
                amount: "1.5".parse().expect("stored total"),
            },
        })
        .expect("legacy billing")
        .expect("matching billing breakdown");

    assert_eq!(billing.total_amount.amount.as_str(), "1.5");
    assert_eq!(billing.input_price_per_million.amount.as_str(), "5");
    assert!(!billing.long_context_billing_applied);
}

#[tokio::test]
async fn reset_credit_success_with_invalid_body_should_remain_an_unknown_consume_result() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/codex/rate-limit-reset-credits/consume"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .expect(1)
        .mount(&server)
        .await;
    let (bundle, account_id, _config) = reset_credit_admin(&server).await;

    let error = bundle
        .admin_provider()
        .consume_reset_credit(reset_credit_command(account_id))
        .await
        .expect_err("invalid success body must be ambiguous");

    assert_eq!(error.kind(), ProviderAdminErrorKind::Ambiguous);
    assert_eq!(
        error.message(),
        Some(
            "OpenAI reset-credit consume result is unknown; refresh the credit list before retrying"
        )
    );
}

#[tokio::test]
async fn reset_credit_explicit_http_rejection_should_preserve_the_raw_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/codex/rate-limit-reset-credits/consume"))
        .respond_with(ResponseTemplate::new(409).set_body_raw(
            r#"{"code":"nothing_to_reset","detail":"window is fresh"}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let (bundle, account_id, _config) = reset_credit_admin(&server).await;

    let error = bundle
        .admin_provider()
        .consume_reset_credit(reset_credit_command(account_id))
        .await
        .expect_err("explicit upstream rejection");

    assert_eq!(error.kind(), ProviderAdminErrorKind::BadGateway);
    assert_eq!(
        error.message(),
        Some(
            r#"OpenAI reset-credit upstream returned HTTP 409: {"code":"nothing_to_reset","detail":"window is fresh"}"#
        )
    );
    let debug = format!("{error:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("window is fresh"));
}

#[tokio::test]
async fn openai_core_provider_projects_codex_request_observation_without_routing_side_effects() {
    let config = valid_config();
    let bundle = provider_openai::initialize(config.config.clone(), provider_ports())
        .await
        .expect("OpenAI bundle");
    let payload = ProtocolPayload::json_object(
        "openai",
        Map::from_iter([
            ("model".to_owned(), json!("gpt-5.4")),
            ("input".to_owned(), json!("summarize")),
            ("reasoning".to_owned(), json!({"effort": "high"})),
        ]),
    )
    .expect("OpenAI payload")
    .with_context(Map::from_iter([(
        "turn_metadata".to_owned(),
        Value::String(r#"{"request_kind":"compaction","subagent_kind":"review"}"#.to_owned()),
    )]));
    let operation = Operation::Generate(GenerateRequest::from_protocol_payload(payload));

    let client_key_id = ClientApiKeyId::new("key_openai_admin_observation").expect("client key");
    let observation = bundle
        .core_provider()
        .request_observation(&operation, &client_key_id);

    assert_eq!(observation.request_kind.as_deref(), Some("compaction"));
    assert_eq!(observation.subagent_kind.as_deref(), Some("review"));
    // Codex 当前只在特定多代理预设组合下给出 reasoning_preset；普通 high 保持空值。
    assert_eq!(observation.reasoning_preset, None);
    assert!(observation.compact);
}

#[tokio::test]
async fn openai_admin_provider_persists_the_full_pending_envelope_and_binds_owner() {
    let pending = Arc::new(TestOAuthPending::default());
    let config = valid_config();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(
            Arc::new(MemoryAccountStore::default()),
            Arc::clone(&pending),
        ),
    )
    .await
    .expect("OpenAI bundle");
    let start_context = MutationContext {
        actor: MutationActor::AdminSession {
            admin_user_id: "admin-owner".to_owned(),
        },
        request_id: "request-start".to_owned(),
    };
    let started = bundle
        .admin_provider()
        .start_authorization(PendingAuthorizationMutation::new(
            ProviderKind::new("openai").expect("provider"),
            AuthorizationMutationTarget::Create {
                name: "OAuth account".to_owned(),
            },
            AuthorizationOwnerBinding::from_context(&start_context),
        ))
        .await
        .expect("start authorization");
    {
        let values = pending.values.lock().expect("OAuth pending");
        let (_, payload, _, _) = values.values().next().expect("stored pending");
        let mutation = payload
            .expose_to_provider()
            .get("mutation")
            .and_then(Value::as_object)
            .expect("pending mutation");
        assert!(
            payload
                .expose_to_provider()
                .get("reauthorization_credential_revision")
                .is_none()
        );
        assert!(
            payload
                .expose_to_provider()
                .get("installation_id")
                .and_then(Value::as_str)
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .is_some_and(|value| value.get_version_num() == 4)
        );
        assert_eq!(
            mutation.get("schema_version").and_then(Value::as_u64),
            Some(3)
        );
        assert!(mutation.get("expected_config_revision").is_none());
        assert!(
            mutation
                .get("target")
                .and_then(Value::as_object)
                .is_some_and(|target| target.get("expected_credential_revision").is_none())
        );
        assert_eq!(
            mutation.get("started_request_id").and_then(Value::as_str),
            Some("request-start")
        );
    }
    let error = bundle
        .admin_provider()
        .complete_authorization(CompleteAuthorization {
            settings: None,
            context: MutationContext {
                actor: MutationActor::AdminSession {
                    admin_user_id: "different-owner".to_owned(),
                },
                request_id: "request-complete".to_owned(),
            },
            flow_id: started.flow_id,
            callback_url: "http://localhost:1455/auth/callback?code=unused&state=unused".to_owned(),
        })
        .await
        .expect_err("wrong owner");
    assert_eq!(error.kind(), ProviderAdminErrorKind::NotFound);
    assert_eq!(pending.values.lock().expect("OAuth pending").len(), 1);
}

#[tokio::test]
async fn openai_reauthorization_pending_payload_reuses_the_account_installation_id() {
    let accounts = Arc::new(MemoryAccountStore::default());
    accounts
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_pending_reauth".to_owned(),
            name: "pending reauthorization".to_owned(),
            secret: secret("pending-reauth-access"),
            verified_account: profile("chatgpt-pending-reauth"),
            next_refresh_at: Some(Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    let account_id = ProviderAccountId::new("acct_pending_reauth").expect("account id");
    let existing = accounts
        .load_current_credential(&account_id)
        .await
        .expect("seeded credential");
    let expected_installation_id = CodexCredentialCodec::decode(&existing.credential)
        .expect("decode seeded credential")
        .installation_id;
    let pending = Arc::new(TestOAuthPending::default());
    let config = valid_config();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(accounts, Arc::clone(&pending)),
    )
    .await
    .expect("OpenAI bundle");
    let context = MutationContext {
        actor: MutationActor::AdminApiKey,
        request_id: "request-pending-reauth".to_owned(),
    };

    bundle
        .admin_provider()
        .start_authorization(PendingAuthorizationMutation::new(
            ProviderKind::new("openai").expect("provider"),
            AuthorizationMutationTarget::Reauthorize { account_id },
            AuthorizationOwnerBinding::from_context(&context),
        ))
        .await
        .expect("start reauthorization");

    let values = pending.values.lock().expect("OAuth pending");
    let (_, payload, _, _) = values.values().next().expect("stored pending");
    let document = payload.expose_to_provider();
    let mutation = document
        .get("mutation")
        .and_then(Value::as_object)
        .expect("pending mutation");
    let target = mutation
        .get("target")
        .and_then(Value::as_object)
        .expect("pending target");
    assert_eq!(
        mutation.get("schema_version").and_then(Value::as_u64),
        Some(3)
    );
    assert_eq!(
        document
            .get("reauthorization_account_id")
            .and_then(Value::as_str),
        Some("acct_pending_reauth")
    );
    assert_eq!(
        document.get("installation_id").and_then(Value::as_str),
        Some(expected_installation_id.as_str())
    );
    assert!(
        document
            .get("reauthorization_credential_revision")
            .is_none()
    );
    assert_eq!(
        target.get("kind").and_then(Value::as_str),
        Some("reauthorize")
    );
    assert_eq!(
        target.get("account_id").and_then(Value::as_str),
        Some("acct_pending_reauth")
    );
    assert!(target.get("expected_credential_revision").is_none());
}

#[tokio::test]
async fn openai_admin_provider_projects_cached_quota_models_and_canonical_export() {
    let store = Arc::new(MemoryAccountStore::default());
    let mut oauth_secret = secret("admin-projection-access");
    oauth_secret.id_token = Some(SecretString::from("header.id-token.signature"));
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_admin_projection".to_owned(),
            name: "admin projection".to_owned(),
            secret: oauth_secret,
            verified_account: profile("chatgpt-admin-projection"),
            next_refresh_at: Some(chrono::Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    let account = store
        .account("acct_admin_projection")
        .expect("stored account");
    let record = account_record(&account);
    let config = valid_config();
    let catalog_cache = Arc::new(TestCatalogCache::default());
    catalog_cache.seed("plan:pro", ["gpt-5.4"]);
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with_catalog(
            Arc::clone(&store),
            Arc::new(TestOAuthPending::default()),
            catalog_cache,
        ),
    )
    .await
    .expect("OpenAI bundle");
    let admin = bundle.admin_provider();

    let operation = admin
        .connection_test_operation(
            &UpstreamModelId::new("gpt-5.4").expect("upstream model"),
            "Reply with exactly OK.",
        )
        .expect("connection test operation");
    let Operation::Generate(request) = operation else {
        panic!("connection test must be a generate operation");
    };
    let encoded = provider_openai::encode_generate_request(&request, "gpt-5.4", None)
        .expect("official OpenAI request");
    assert_eq!(
        encoded.body().get("model").and_then(Value::as_str),
        Some("gpt-5.4")
    );
    assert_eq!(
        encoded.body().get("stream").and_then(Value::as_bool),
        Some(true)
    );

    let account_id = account.id().clone();
    let quota = admin
        .quota(ProviderQuotaRequest {
            account_id: account_id.clone(),
            refresh: false,
            rolling_usage: None,
        })
        .await
        .expect("cached quota");
    assert!(quota.windows.is_empty());
    let models = admin
        .models(&account_id, false)
        .await
        .expect("cached models");
    assert_eq!(models.models[0].id.as_str(), "gpt-5.4");
    let loaded = store
        .load_credential(account.id(), account.revision())
        .await
        .expect("loaded credential");
    let exported = admin
        .export_credentials(vec![ProviderExportCredentialInput {
            account: record,
            provider_material: ProviderDocument::new(OpaqueProviderData::new(
                loaded.credential.into_inner(),
            )),
        }])
        .await
        .expect("canonical export");
    assert_eq!(exported.account_ids, vec![account_id]);
    let document = exported.document.expose_to_provider().expose_to_provider();
    assert_eq!(
        document.get("sourceFormat").and_then(Value::as_str),
        Some("cpr")
    );
    let exported_account = document
        .get("accounts")
        .and_then(Value::as_array)
        .and_then(|accounts| accounts.first())
        .expect("exported OAuth account");
    assert_eq!(
        exported_account.get("accessToken").and_then(Value::as_str),
        Some("admin-projection-access")
    );
    assert_eq!(
        exported_account.get("idToken").and_then(Value::as_str),
        Some("header.id-token.signature")
    );
    assert!(exported_account.get("token").is_none());
}

#[tokio::test]
async fn turn_state_probe_short_circuits_after_first_success_and_applies_state() {
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_turn_state_probe".to_owned(),
            name: "turn state probe".to_owned(),
            secret: secret("turn-state-probe-access"),
            verified_account: profile("chatgpt-turn-state-probe"),
            next_refresh_at: Some(Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    let account = store
        .account("acct_turn_state_probe")
        .expect("turn state account");
    let server = MockServer::start().await;
    let state = "s".repeat(292);
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", state)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(COMPLETED_SESSION_SSE),
        )
        .expect(2)
        .mount(&server)
        .await;
    let mut config = valid_config();
    config.config.api.base_url = server.uri();
    let capture = Arc::new(TestTurnStateProbeCapture::default());
    let bundle = provider_openai::initialize_with_turn_state_capture(
        config.config.clone(),
        provider_ports_with(store, Arc::new(TestOAuthPending::default())),
        capture.clone(),
    )
    .await
    .expect("OpenAI turn state bundle");
    let admin = bundle.admin_provider();
    let model = UpstreamModelId::new("gpt-5.4").expect("upstream model");
    let proxy = OutboundProxy::parse(&server.uri()).expect("probe proxy");

    let result = admin
        .probe_turn_state(
            account.id(),
            &model,
            vec![
                TurnStateProbeTarget {
                    id: "saved-egress-a".to_owned(),
                    label: "已保存出口 A".to_owned(),
                    proxy: proxy.clone(),
                },
                TurnStateProbeTarget {
                    id: "saved-egress-b".to_owned(),
                    label: "已保存出口 B".to_owned(),
                    proxy,
                },
            ],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("turn state probe");

    assert_eq!(result.model, "gpt-5.4");
    assert_eq!(result.attempts.len(), 1);
    assert!(result.attempts.iter().all(|attempt| attempt.state_acquired));
    assert!(result.attempts[0].exchange_id.is_some());
    let captures = capture.take();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].status_code, Some(200));
    assert_eq!(captures[0].outcome, "state_acquired");
    assert!(
        captures[0].request_headers.iter().any(|header| {
            header.name == "content-encoding" && header.value.as_slice() == b"zstd"
        })
    );
    assert!(
        captures[0]
            .response_headers
            .iter()
            .any(|header| { header.name == "x-codex-turn-state" && header.value.len() == 292 })
    );
    let snapshot = admin
        .turn_state_snapshot(account.id(), &model)
        .expect("turn state snapshot");
    assert_eq!(snapshot.probe_history, vec![result]);
    let overview = admin.turn_state_overview();
    assert_eq!(overview.len(), 1);
    let overview = &overview[0];
    assert_eq!(overview.account_id, "acct_turn_state_probe");
    assert_eq!(overview.model, "gpt-5.4");
    assert!(overview.state_available);
    assert!(overview.state_captured_at.is_some());
    assert!(overview.state_first_applied_at.is_none());
    assert!(overview.state_expires_at.is_some());
    assert!(overview.next_rotation_at.is_some());
    assert_eq!(overview.state_source, Some(TurnStateSource::ManualProbe));
    assert!(overview.latest_probe_succeeded);
    assert_eq!(overview.latest_probe_attempt_count, 1);
    let active_target_id = overview
        .latest_probe_active_target_id
        .as_deref()
        .expect("overview active target");
    let active_attempt = snapshot.probe_history[0]
        .attempts
        .iter()
        .find(|attempt| attempt.target_id == active_target_id)
        .expect("active target attempt");
    assert_eq!(
        overview.latest_probe_active_target_label.as_deref(),
        Some(active_attempt.target_label.as_str())
    );

    let payload = ProtocolPayload::json_object(
        "openai",
        Map::from_iter([
            ("model".to_owned(), json!("gpt-5.4")),
            ("input".to_owned(), json!("apply cached state")),
        ]),
    )
    .expect("probe follow-up payload")
    .with_context(Map::from_iter([("use_websocket".to_owned(), json!(false))]));
    let operation = Operation::Generate(GenerateRequest::from_protocol_payload(payload));
    let mut stream = bundle
        .core_provider()
        .execute(
            initialized_provider_request(operation, account.id().as_str()),
            initialized_attempt_context("req_apply_turn_state", account.id().as_str()),
        )
        .await
        .expect("prepare request with turn state");
    while let Some(event) = stream.next().await {
        event.expect("turn state follow-up response");
    }
    let applied_snapshot = admin
        .turn_state_snapshot(account.id(), &model)
        .expect("applied turn state snapshot");
    assert!(applied_snapshot.state_first_applied_at.is_some());
    assert_eq!(
        applied_snapshot.state_source,
        Some(TurnStateSource::ManualProbe)
    );

    let requests = server
        .received_requests()
        .await
        .expect("turn state probe requests");
    assert_eq!(requests.len(), 2);
    assert!({
        let request = &requests[0];
        let body = request
            .headers
            .get("content-encoding")
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.eq_ignore_ascii_case("zstd"))
            .and_then(|_| {
                zstd::stream::decode_all(std::io::Cursor::<&[u8]>::new(request.body.as_ref())).ok()
            })
            .and_then(|body| serde_json::from_slice::<Value>(&body).ok());
        body.as_ref()
            .and_then(|body| body.get("model").and_then(Value::as_str))
            == Some("gpt-5.4")
            && body
                .as_ref()
                .and_then(|body| body.get("stream").and_then(Value::as_bool))
                == Some(true)
            && body
                .as_ref()
                .and_then(|body| body.get("input"))
                .and_then(Value::as_array)
                .and_then(|input| input.first())
                .and_then(|item| item.get("type").and_then(Value::as_str))
                == Some("message")
    });
    assert_eq!(
        requests[1]
            .headers
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok())
            .map(str::len),
        Some(292)
    );
}

#[tokio::test]
async fn turn_state_smart_scheduling_prefers_exact_model_and_keeps_business_refresh() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let ids = ["acct_state_cold", "acct_state_ready"];
    let (bundle, store) = turn_state_fixture(&base, &ids).await;
    let ready = store.account(ids[1]).expect("ready account");
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(probe_response(200, Some('p')))
        .expect(1)
        .mount(&proxy)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(COMPLETED_SESSION_SSE),
        )
        .expect(4)
        .mount(&base)
        .await;
    bundle
        .admin_provider()
        .probe_turn_state(
            ready.id(),
            &upstream_model("gpt-5.4"),
            vec![probe_target("probe", &proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("probe state");

    for index in 0..2 {
        assert_eq!(
            select_turn_state_business(
                &bundle,
                &ids,
                "gpt-5.4",
                &format!("req_state_priority_{index}"),
                true
            )
            .await,
            ids[1]
        );
    }
    let mut other_model_accounts = BTreeSet::new();
    for index in 0..2 {
        other_model_accounts.insert(
            select_turn_state_business(
                &bundle,
                &ids,
                "gpt-other",
                &format!("req_state_other_{index}"),
                true,
            )
            .await,
        );
    }
    assert_eq!(
        other_model_accounts,
        ids.map(str::to_owned).into_iter().collect()
    );
    let requests = base.received_requests().await.expect("business requests");
    for request in &requests[..2] {
        assert_eq!(
            request
                .headers
                .get("x-codex-turn-state")
                .and_then(|value| value.to_str().ok()),
            Some("p".repeat(292).as_str())
        );
    }
    for request in &requests[2..] {
        assert!(!request.headers.contains_key("x-codex-turn-state"));
    }
    base.verify().await;
    base.reset().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "b".repeat(292))
                .insert_header("content-type", "text/event-stream")
                .set_body_string(COMPLETED_SESSION_SSE),
        )
        .expect(2)
        .mount(&base)
        .await;
    for index in 0..2 {
        assert_eq!(
            select_turn_state_business(
                &bundle,
                &ids,
                "gpt-5.4",
                &format!("req_state_refresh_{index}"),
                true
            )
            .await,
            ids[1]
        );
    }
    assert_eq!(
        bundle
            .admin_provider()
            .turn_state_snapshot(ready.id(), &upstream_model("gpt-5.4"))
            .expect("refreshed state")
            .state_source,
        Some(TurnStateSource::UpstreamResponse)
    );
    let requests = base.received_requests().await.expect("refreshed requests");
    assert_eq!(
        requests[1]
            .headers
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok()),
        Some("b".repeat(292).as_str())
    );
}

#[tokio::test]
async fn turn_state_smart_scheduling_falls_back_on_lease_busy_and_invalidated_state() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let ids = ["acct_state_cold", "acct_state_ready"];
    let leases = Arc::new(TestLeaseCoordinator::default());
    let (bundle, store) = turn_state_fixture_with_leases(&base, &ids, Arc::clone(&leases)).await;
    let ready = store.account(ids[1]).expect("ready account");
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(probe_response(200, Some('p')))
        .expect(1)
        .mount(&proxy)
        .await;
    bundle
        .admin_provider()
        .probe_turn_state(
            ready.id(),
            &upstream_model("gpt-5.4"),
            vec![probe_target("probe", &proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("probe state");
    let original = bundle
        .admin_provider()
        .turn_state_snapshot(ready.id(), &upstream_model("gpt-5.4"))
        .expect("probe snapshot");
    let completed = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(COMPLETED_SESSION_SSE);
    mount_turn_state_sequence(
        &base,
        Arc::new(Notify::new()),
        vec![
            completed.clone(),
            probe_response(312, None),
            completed.clone(),
            completed,
        ],
    )
    .await;
    leases
        .busy_accounts
        .lock()
        .expect("busy accounts")
        .insert(ready.id().clone());
    assert_eq!(
        select_turn_state_business(&bundle, &ids, "gpt-5.4", "req_state_lease_busy", true).await,
        ids[0]
    );
    let after_selection = bundle
        .admin_provider()
        .turn_state_snapshot(ready.id(), &upstream_model("gpt-5.4"))
        .expect("unapplied state");
    assert_eq!(after_selection.state_expires_at, original.state_expires_at);
    assert!(after_selection.state_first_applied_at.is_none());
    let attempts = leases
        .requests
        .lock()
        .expect("lease attempts")
        .iter()
        .map(|request| request.account_id().to_string())
        .collect::<Vec<_>>();
    assert_eq!(attempts, [ids[1], ids[0]]);
    leases.busy_accounts.lock().expect("busy accounts").clear();
    assert_eq!(
        select_turn_state_business(&bundle, &ids, "gpt-5.4", "req_state_312", false).await,
        ids[1]
    );
    assert!(
        !bundle
            .admin_provider()
            .turn_state_overview()
            .iter()
            .find(|entry| entry.account_id == ids[1])
            .expect("invalidated state")
            .state_available
    );
    let mut accounts = BTreeSet::new();
    for index in 0..2 {
        accounts.insert(
            select_turn_state_business(
                &bundle,
                &ids,
                "gpt-5.4",
                &format!("req_state_invalidated_{index}"),
                true,
            )
            .await,
        );
    }
    assert_eq!(accounts, ids.map(str::to_owned).into_iter().collect());
    let requests = base.received_requests().await.expect("business requests");
    assert!(!requests[0].headers.contains_key("x-codex-turn-state"));
    for request in &requests[2..] {
        assert!(!request.headers.contains_key("x-codex-turn-state"));
    }
}

async fn select_turn_state_business(
    bundle: &provider_openai::ProviderBundle,
    account_ids: &[&str],
    model: &str,
    request_id: &str,
    expect_success: bool,
) -> String {
    let scope = Arc::new(FrozenAccountScope::new(
        Arc::new(RuntimeAccountDirectory::new(
            account_ids
                .iter()
                .map(|id| {
                    (
                        ProviderAccountId::new(*id).expect("account ID"),
                        RuntimeAccount::new(
                            ProviderKind::new("openai").expect("provider"),
                            BTreeSet::new(),
                        ),
                    )
                })
                .collect(),
        )),
        ClientRoutingScope::all_accounts(),
    ));
    let payload = ProtocolPayload::json_object(
        "openai",
        Map::from_iter([
            ("model".to_owned(), json!("client-model-alias")),
            ("input".to_owned(), json!("state scheduling")),
        ]),
    )
    .expect("payload")
    .with_context(Map::from_iter([("use_websocket".to_owned(), json!(false))]));
    let mut stream = bundle
        .core_provider()
        .execute(
            initialized_provider_request_for_scope(
                Operation::Generate(GenerateRequest::from_protocol_payload(payload)),
                Arc::clone(&scope),
                model,
            ),
            initialized_attempt_context_for_scope(request_id, scope),
        )
        .await
        .expect("prepare scheduled request");
    let selected = stream.metadata().provider_account_id().to_string();
    let mut failed = false;
    while let Some(event) = stream.next().await {
        if expect_success {
            event.expect("business response");
        } else {
            failed |= event.is_err();
        }
    }
    assert_eq!(!failed, expect_success);
    selected
}

#[tokio::test]
async fn turn_state_probe_enforces_manual_and_automatic_budgets_and_rotates_candidates() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let proxy_count = mount_turn_state_sequence(
        &proxy,
        Arc::clone(&seen),
        vec![
            probe_response(200, None),
            probe_response(200, None),
            probe_response(200, None),
            probe_response(200, Some('d')),
            probe_response(200, None),
        ],
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(312).set_body_json(json!({"error": "stale"})))
        .expect(1)
        .mount(&base)
        .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_probe_budget"]).await;
    let account = store.account("acct_probe_budget").expect("budget account");
    let model = upstream_model("gpt-5.4");
    let targets = probe_targets(&proxy, &["a", "b", "c", "d", "e"]);
    let _clock = PausedTimeGuard::new();

    let manual_bundle = Arc::clone(&bundle);
    let manual_account = account.id().clone();
    let manual_model = model.clone();
    let manual_targets = targets.clone();
    let manual = tokio::spawn(async move {
        manual_bundle
            .admin_provider()
            .probe_turn_state(
                &manual_account,
                &manual_model,
                manual_targets,
                TurnStateSource::ManualProbe,
            )
            .await
    });
    wait_for_request(&seen, &proxy_count, 1).await;
    tokio::time::advance(Duration::from_secs(9)).await;
    assert_eq!(proxy_count.load(Ordering::SeqCst), 1);
    assert!(!manual.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for_request(&seen, &proxy_count, 2).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    wait_for_request(&seen, &proxy_count, 3).await;
    let failed = manual.await.expect("manual task").expect("manual result");
    assert_eq!(
        failed
            .attempts
            .iter()
            .map(|attempt| attempt.target_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    assert!(failed.active_target_id.is_none());

    let delay = turn_state_backoff("acct_probe_budget", Some("gpt-5.4"), 1);
    tokio::time::advance(delay - Duration::from_secs(1)).await;
    let blocked = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            targets.clone(),
            TurnStateSource::ManualProbe,
        )
        .await
        .expect_err("key backoff remains active one second before its boundary");
    assert_eq!(blocked.kind(), ProviderAdminErrorKind::Unavailable);
    assert_eq!(proxy_count.load(Ordering::SeqCst), 3);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(
        bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &upstream_model("other"),
                targets.clone(),
                TurnStateSource::ManualProbe
            )
            .await
            .is_err()
    );
    // 三次请求消耗账号预算；键退避结束也不能绕过跨模型的滚动窗口。
    tokio::time::advance(Duration::from_secs(279) - delay).await;
    assert!(
        bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &model,
                targets.clone(),
                TurnStateSource::ManualProbe
            )
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    let recovered = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            targets.clone(),
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("next manual round");
    assert_eq!(recovered.active_target_id.as_deref(), Some("d"));
    assert_eq!(recovered.attempts.len(), 1);
    assert_eq!(proxy_count.load(Ordering::SeqCst), 4);

    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        "gpt-5.4",
        "req_budget_invalidate",
        false,
    )
    .await;
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(bundle.admin_provider().due_turn_state_subjects().len(), 1);

    store.set_egress(
        account.id().as_str(),
        Some(OutboundProxy::parse(&proxy.uri()).expect("bound proxy")),
        None,
    );
    let automatic_bundle = Arc::clone(&bundle);
    let automatic_account = account.id().clone();
    let automatic_model = model.clone();
    let automatic = tokio::spawn(async move {
        automatic_bundle
            .admin_provider()
            .probe_turn_state(
                &automatic_account,
                &automatic_model,
                targets,
                TurnStateSource::AutomaticRenewal,
            )
            .await
    });
    wait_for_request(&seen, &proxy_count, 5).await;
    let automatic = automatic
        .await
        .expect("automatic task")
        .expect("automatic result");
    assert_eq!(automatic.attempts.len(), 1);
    assert_eq!(automatic.attempts[0].target_id, "d");
    assert_eq!(
        proxy
            .received_requests()
            .await
            .expect("proxy requests")
            .len(),
        5
    );
    assert_eq!(
        base.received_requests()
            .await
            .expect("business requests")
            .len(),
        1
    );
    assert!(bundle.admin_provider().due_turn_state_subjects().is_empty());
    let automatic_backoff = turn_state_backoff("acct_probe_budget", Some("gpt-5.4"), 1);
    let due_after = automatic_backoff;
    tokio::time::advance(due_after - Duration::from_secs(1)).await;
    assert!(bundle.admin_provider().due_turn_state_subjects().is_empty());
    assert_eq!(proxy_count.load(Ordering::SeqCst), 5);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(bundle.admin_provider().due_turn_state_subjects().len(), 1);
}

#[tokio::test]
async fn turn_state_renewal_requires_business_activity_and_expires_when_idle() {
    let base = MockServer::start().await;
    let business_seen = Arc::new(Notify::new());
    mount_turn_state_sequence(
        &base,
        business_seen,
        vec![
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(COMPLETED_SESSION_SSE),
            probe_response(312, None),
            probe_response(312, None),
        ],
    )
    .await;
    let proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    mount_turn_state_sequence(
        &proxy,
        seen,
        vec![
            probe_response(200, Some('a')),
            probe_response(312, None),
            probe_response(200, Some('b')),
            probe_response(312, None),
        ],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_activity"]).await;
    let account = store.account("acct_activity").expect("account");
    let model = upstream_model("gpt-5.4");
    let target = probe_target("bound", &proxy);
    let _clock = PausedTimeGuard::new();
    let admin = bundle.admin_provider();
    admin
        .probe_turn_state(
            account.id(),
            &model,
            vec![target.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("manual seed");
    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        model.as_str(),
        "req_activity_apply",
        true,
    )
    .await;
    assert!(
        admin
            .turn_state_snapshot(account.id(), &model)
            .expect("snapshot")
            .state_first_applied_at
            .is_some()
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    admin
        .probe_turn_state(
            account.id(),
            &model,
            vec![target.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("invalidate applied state");
    tokio::time::advance(Duration::from_secs(300)).await;
    assert_eq!(admin.due_turn_state_subjects().len(), 1);
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert!(
        admin.due_turn_state_subjects().is_empty(),
        "idle models stop warming"
    );
    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        model.as_str(),
        "req_activity_resume",
        false,
    )
    .await;
    assert_eq!(admin.due_turn_state_subjects().len(), 1);
    // 无匹配出口不能用别的 IP 续采；随后绑定当前候选再恢复。
    let skipped = admin
        .probe_turn_state(
            account.id(),
            &model,
            vec![target.clone()],
            TurnStateSource::AutomaticRenewal,
        )
        .await
        .expect("unbound renewal");
    assert!(skipped.attempts.is_empty());
    store.set_egress(account.id().as_str(), Some(target.proxy.clone()), None);
    let renewed = admin
        .probe_turn_state(
            account.id(),
            &model,
            vec![target.clone()],
            TurnStateSource::AutomaticRenewal,
        )
        .await
        .expect("renew active state");
    assert!(renewed.active_target_id.is_some());
    tokio::time::advance(Duration::from_secs(10)).await;
    admin
        .probe_turn_state(
            account.id(),
            &model,
            vec![target],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("invalidate unused replacement");
    tokio::time::advance(Duration::from_secs(300)).await;
    assert!(
        admin.due_turn_state_subjects().is_empty(),
        "replacement needs business application"
    );
    store.set_egress(account.id().as_str(), None, None);
    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        model.as_str(),
        "req_activity_new_312",
        false,
    )
    .await;
    assert_eq!(admin.due_turn_state_subjects().len(), 1);
}

#[tokio::test]
async fn turn_state_manual_success_does_not_enable_renewal_and_bound_proxy_wins() {
    let base = MockServer::start().await;
    let proxy_a = MockServer::start().await;
    let proxy_b = MockServer::start().await;
    mount_turn_state_sequence(
        &proxy_a,
        Arc::new(Notify::new()),
        vec![probe_response(200, Some('a'))],
    )
    .await;
    mount_turn_state_sequence(
        &proxy_b,
        Arc::new(Notify::new()),
        vec![probe_response(401, None), probe_response(312, None)],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_bound"]).await;
    let account = store.account("acct_bound").expect("account");
    let model = upstream_model("gpt-5.4");
    let a = probe_target("a", &proxy_a);
    let b = probe_target("b", &proxy_b);
    let _clock = PausedTimeGuard::new();
    let admin = bundle.admin_provider();
    admin
        .probe_turn_state(
            account.id(),
            &model,
            vec![a.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("seed preferred a");
    store.set_egress(account.id().as_str(), Some(b.proxy.clone()), None);
    tokio::time::advance(Duration::from_secs(10)).await;
    let result = admin
        .probe_turn_state(
            account.id(),
            &model,
            vec![a, b.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("bound b before preferred a");
    assert_eq!(result.attempts[0].target_id, "b");
    assert_eq!(result.attempts.len(), 1);
    tokio::time::advance(Duration::from_secs(300)).await;
    admin
        .probe_turn_state(account.id(), &model, vec![b], TurnStateSource::ManualProbe)
        .await
        .expect("invalidate unused manual state");
    tokio::time::advance(Duration::from_secs(300)).await;
    assert!(
        admin.due_turn_state_subjects().is_empty(),
        "manual collection never grants eligibility"
    );
}

fn prepare_isolated_turn_state_configuration_case(test_name: &str) -> Option<std::path::PathBuf> {
    const CASE_ENV: &str = "CODEX_PROXY_TEST_TURN_STATE_CONFIGURATION_CASE";
    const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBnTCCAUOgAwIBAgIUP8Kl1Dph20iorPXhwAHDWzhknfIwCgYIKoZIzj0EAwIw\n\
IzEhMB8GA1UEAwwYdHVybi1zdGF0ZS1wcm9iZS10ZXN0LWNhMCAXDTI2MDkxOTA0\n\
NTczOFoYDzIxMjYwODI2MDQ1NzM4WjAjMSEwHwYDVQQDDBh0dXJuLXN0YXRlLXBy\n\
b2JlLXRlc3QtY2EwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAATs4tvVA7pB9itU\n\
a4LReI/PcYqy2tLp8gl9qvA84JheDIMER6h85YoiJQ/09tno23iPZaMc20EANHWt\n\
3nUJV+iMo1MwUTAdBgNVHQ4EFgQUN0ZhVastzLLjGa0K3k5Z8zsJLUUwHwYDVR0j\n\
BBgwFoAUN0ZhVastzLLjGa0K3k5Z8zsJLUUwDwYDVR0TAQH/BAUwAwEB/zAKBggq\n\
hkjOPQQDAgNIADBFAiAoDIu7BJTB/mCglHqBD7T+wBwLgVgWkwE7HRNWRTz+fgIh\n\
AKtkELs+VxJyFlYnWpXY4NPndkNuhyR6ELY1UFJ8V4om\n\
-----END CERTIFICATE-----\n";

    if let Some(selected_case) = std::env::var_os(CASE_ENV) {
        assert_eq!(selected_case, test_name);
        let ca_path = std::path::PathBuf::from(
            std::env::var_os("CODEX_CA_CERTIFICATE").expect("isolated test CA path"),
        );
        assert!(!ca_path.exists(), "test CA must start absent");
        assert!(
            std::env::var_os("SSL_CERT_FILE").is_none(),
            "system CA override must stay disabled"
        );
        std::fs::write(&ca_path, TEST_CA_PEM).expect("write isolated test CA");
        return Some(ca_path);
    }

    // CA 环境变量和 HTTP 客户端缓存都属于进程级状态；子进程保证两个场景互不污染。
    let runtime = tempfile::tempdir().expect("isolated turn state runtime");
    let ca_path = runtime.path().join("turn-state-ca.pem");
    assert!(!ca_path.exists(), "parent must pass an absent CA path");
    let output = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .arg("--exact")
        .arg(format!("admin::{test_name}"))
        .arg("--nocapture")
        .env(CASE_ENV, test_name)
        .env("CODEX_CA_CERTIFICATE", &ca_path)
        .env_remove("SSL_CERT_FILE")
        .output()
        .expect("run isolated turn state case");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "isolated case failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let completed = format!("turn-state-configuration-case-completed:{test_name}");
    // 精确完成标记同时防止 --exact 过滤错误造成零测试假成功。
    assert_eq!(
        stdout.lines().filter(|line| *line == completed).count(),
        1,
        "isolated case did not complete exactly once\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    None
}

#[tokio::test]
async fn turn_state_probe_configuration_failures_consume_budget_without_history() {
    const CASE_NAME: &str =
        "turn_state_probe_configuration_failures_consume_budget_without_history";
    let Some(ca_path) = prepare_isolated_turn_state_configuration_case(CASE_NAME) else {
        return;
    };
    let parked_ca_path = ca_path.with_extension("parked");
    let base = MockServer::start().await;
    let valid_proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let proxy_count = mount_turn_state_sequence(
        &valid_proxy,
        Arc::clone(&seen),
        vec![probe_response(200, Some('v'))],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_proxy_configuration"]).await;
    let account = store
        .account("acct_proxy_configuration")
        .expect("configuration account");
    let targets = probe_targets(&valid_proxy, &["a", "b", "c", "d"]);
    let _clock = PausedTimeGuard::new();

    // 初始化后移走 CA，使尚未缓存的候选客户端在配置阶段失败且不会发出代理请求。
    assert!(!parked_ca_path.exists(), "parked CA must start absent");
    std::fs::rename(&ca_path, &parked_ca_path).expect("park isolated test CA");
    assert!(!ca_path.exists(), "configured CA path must be absent");
    let error = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-config"),
            targets.clone(),
            TurnStateSource::ManualProbe,
        )
        .await
        .expect_err("three configuration failures exhaust manual budget");
    assert_eq!(error.kind(), ProviderAdminErrorKind::Unavailable);
    assert_eq!(proxy_count.load(Ordering::SeqCst), 0);
    assert!(
        valid_proxy
            .received_requests()
            .await
            .expect("valid proxy requests")
            .is_empty()
    );
    assert!(
        base.received_requests()
            .await
            .expect("base requests")
            .is_empty()
    );
    let snapshot = bundle
        .admin_provider()
        .turn_state_snapshot(account.id(), &upstream_model("gpt-config"))
        .expect("configuration snapshot");
    assert!(snapshot.probe_history.is_empty());

    std::fs::rename(&parked_ca_path, &ca_path).expect("restore isolated test CA");
    let recovered = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-config"),
            targets,
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("untried candidate remains available next round");
    assert_eq!(recovered.active_target_id.as_deref(), Some("d"));
    assert_eq!(recovered.attempts.len(), 1);
    assert_eq!(recovered.attempts[0].target_id, "d");
    assert_eq!(proxy_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        valid_proxy
            .received_requests()
            .await
            .expect("valid proxy request")
            .len(),
        1
    );
    assert_eq!(
        bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &upstream_model("gpt-config"))
            .expect("recovered snapshot")
            .probe_history
            .len(),
        1
    );
    println!("\nturn-state-configuration-case-completed:{CASE_NAME}");
}

#[tokio::test]
async fn automatic_turn_state_probe_configuration_failures_leave_untried_candidate_for_next_run() {
    const CASE_NAME: &str =
        "automatic_turn_state_probe_configuration_failures_leave_untried_candidate_for_next_run";
    let Some(ca_path) = prepare_isolated_turn_state_configuration_case(CASE_NAME) else {
        return;
    };
    let parked_ca_path = ca_path.with_extension("parked");
    let base = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(312).set_body_json(json!({"error": "stale"})))
        .expect(1)
        .mount(&base)
        .await;
    let valid_proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let valid_count = mount_turn_state_sequence(
        &valid_proxy,
        Arc::clone(&seen),
        vec![probe_response(200, Some('r'))],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_auto_configuration"]).await;
    let account = store
        .account("acct_auto_configuration")
        .expect("automatic configuration account");
    let model = upstream_model("gpt-auto-config");
    let targets = probe_targets(&valid_proxy, &["a", "b", "c"]);
    let _clock = PausedTimeGuard::new();
    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        model.as_str(),
        "req_auto_config_invalidate",
        false,
    )
    .await;
    assert_eq!(valid_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        base.received_requests().await.expect("base requests").len(),
        1
    );
    assert!(
        valid_proxy
            .received_requests()
            .await
            .expect("valid proxy requests")
            .is_empty()
    );
    let history_before = bundle
        .admin_provider()
        .turn_state_snapshot(account.id(), &model)
        .expect("seed snapshot")
        .probe_history
        .len();

    store.set_egress(
        account.id().as_str(),
        Some(OutboundProxy::parse(&valid_proxy.uri()).expect("bound proxy")),
        None,
    );
    // 基础客户端已经创建；移走 CA 只让首轮新建候选客户端失败，避免预热掩盖配置错误。
    assert!(!parked_ca_path.exists(), "parked CA must start absent");
    std::fs::rename(&ca_path, &parked_ca_path).expect("park isolated test CA");
    assert!(!ca_path.exists(), "configured CA path must be absent");
    let first = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            targets.clone(),
            TurnStateSource::AutomaticRenewal,
        )
        .await
        .expect("automatic configuration result");
    assert!(first.attempts.is_empty());
    assert!(first.active_target_id.is_none());
    assert_eq!(valid_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &model)
            .expect("configuration failure snapshot")
            .probe_history
            .len(),
        history_before
    );
    std::fs::rename(&parked_ca_path, &ca_path).expect("restore isolated test CA");
    let second = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            targets,
            TurnStateSource::AutomaticRenewal,
        )
        .await
        .expect("untried automatic candidate");
    assert_eq!(second.active_target_id.as_deref(), Some("b"));
    assert_eq!(second.attempts.len(), 1);
    assert_eq!(second.attempts[0].target_id, "b");
    assert_eq!(valid_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &model)
            .expect("recovered automatic snapshot")
            .probe_history
            .len(),
        history_before + 1
    );
    assert_eq!(
        base.received_requests().await.expect("base requests").len(),
        1
    );
    println!("\nturn-state-configuration-case-completed:{CASE_NAME}");
}

#[tokio::test]
async fn turn_state_probe_prefers_model_then_account_proxy_without_cross_model_state() {
    let base = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(COMPLETED_SESSION_SSE),
        )
        .expect(1)
        .mount(&base)
        .await;
    let proxy_a = MockServer::start().await;
    let proxy_b = MockServer::start().await;
    let seen_a = Arc::new(Notify::new());
    let seen_b = Arc::new(Notify::new());
    let a_count = mount_turn_state_sequence(
        &proxy_a,
        Arc::clone(&seen_a),
        vec![probe_response(200, None), probe_response(200, Some('a'))],
    )
    .await;
    let b_count = mount_turn_state_sequence(
        &proxy_b,
        Arc::clone(&seen_b),
        vec![
            probe_response(200, Some('b')),
            probe_response(200, None),
            probe_response(200, Some('c')),
        ],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_probe_preference"]).await;
    let account = store
        .account("acct_probe_preference")
        .expect("preference account");
    let first_model = upstream_model("gpt-5.4");
    let second_model = upstream_model("gpt-5.3-codex");
    let targets = vec![probe_target("a", &proxy_a), probe_target("b", &proxy_b)];
    let _clock = PausedTimeGuard::new();

    let first_bundle = Arc::clone(&bundle);
    let first_account = account.id().clone();
    let first_model_task = first_model.clone();
    let first_targets = targets.clone();
    let first = tokio::spawn(async move {
        first_bundle
            .admin_provider()
            .probe_turn_state(
                &first_account,
                &first_model_task,
                first_targets,
                TurnStateSource::ManualProbe,
            )
            .await
    });
    wait_for_request(&seen_a, &a_count, 1).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    wait_for_request(&seen_b, &b_count, 1).await;
    let first = first.await.expect("first task").expect("first probe");
    assert_eq!(first.active_target_id.as_deref(), Some("b"));
    assert!(
        bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &second_model)
            .is_none()
    );

    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        second_model.as_str(),
        "req_cross_model_state",
        true,
    )
    .await;
    let base_requests = base.received_requests().await.expect("base requests");
    assert!(base_requests[0].headers.get("x-codex-turn-state").is_none());

    // 本场景验证偏好顺序，先等待首轮请求离开账号预算窗口。
    tokio::time::advance(Duration::from_secs(300)).await;
    let second_bundle = Arc::clone(&bundle);
    let second_account = account.id().clone();
    let second_model_task = second_model.clone();
    let second_targets = targets.clone();
    let second = tokio::spawn(async move {
        second_bundle
            .admin_provider()
            .probe_turn_state(
                &second_account,
                &second_model_task,
                second_targets,
                TurnStateSource::ManualProbe,
            )
            .await
    });
    wait_for_request(&seen_b, &b_count, 2).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    wait_for_request(&seen_a, &a_count, 2).await;
    let second = second.await.expect("second task").expect("second probe");
    assert_eq!(
        second
            .attempts
            .iter()
            .map(|attempt| attempt.target_id.as_str())
            .collect::<Vec<_>>(),
        vec!["b", "a"]
    );

    tokio::time::advance(Duration::from_secs(10)).await;
    let third_bundle = Arc::clone(&bundle);
    let third_account = account.id().clone();
    let third_targets = targets.clone();
    let third = tokio::spawn(async move {
        third_bundle
            .admin_provider()
            .probe_turn_state(
                &third_account,
                &first_model,
                third_targets,
                TurnStateSource::ManualProbe,
            )
            .await
    });
    wait_for_request(&seen_b, &b_count, 3).await;
    let third = third.await.expect("third task").expect("third probe");
    assert_eq!(third.active_target_id.as_deref(), Some("b"));
    assert_eq!(third.attempts.len(), 1);
    assert_eq!(proxy_a.received_requests().await.expect("proxy a").len(), 2);
    assert_eq!(proxy_b.received_requests().await.expect("proxy b").len(), 3);
    assert_eq!(a_count.load(Ordering::SeqCst), 2);
    assert_eq!(b_count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn turn_state_probe_serializes_accounts_and_limits_global_network_concurrency() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let delayed = probe_response(200, Some('s')).set_delay(Duration::from_secs(5));
    let proxy_count = mount_turn_state_sequence(
        &proxy,
        Arc::clone(&seen),
        vec![delayed.clone(), delayed.clone(), delayed],
    )
    .await;
    let (bundle, store) =
        turn_state_fixture(&base, &["acct_slot_a", "acct_slot_b", "acct_slot_c"]).await;
    let target = probe_target("shared", &proxy);
    let _clock = PausedTimeGuard::new();

    let mut tasks = Vec::new();
    for (index, account_id) in ["acct_slot_a", "acct_slot_b", "acct_slot_c"]
        .into_iter()
        .enumerate()
    {
        let task_bundle = Arc::clone(&bundle);
        let account = store
            .account(account_id)
            .expect("slot account")
            .id()
            .clone();
        let task_target = target.clone();
        tasks.push(tokio::spawn(async move {
            task_bundle
                .admin_provider()
                .probe_turn_state(
                    &account,
                    &upstream_model(&format!("gpt-slot-{index}")),
                    vec![task_target],
                    TurnStateSource::ManualProbe,
                )
                .await
        }));
    }
    wait_for_request(&seen, &proxy_count, 1).await;
    wait_for_request(&seen, &proxy_count, 2).await;
    assert_eq!(proxy_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        proxy
            .received_requests()
            .await
            .expect("in-flight requests")
            .len(),
        2
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    wait_for_request(&seen, &proxy_count, 3).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    for task in tasks {
        let result = task.await.expect("slot task").expect("slot probe");
        assert_eq!(result.attempts.len(), 1);
        assert!(result.attempts[0].state_acquired);
    }
    assert_eq!(
        proxy.received_requests().await.expect("all requests").len(),
        3
    );

    tokio::time::advance(Duration::from_secs(10)).await;
    let serial_proxy = MockServer::start().await;
    let serial_seen = Arc::new(Notify::new());
    let serial_count = mount_turn_state_sequence(
        &serial_proxy,
        Arc::clone(&serial_seen),
        vec![probe_response(200, Some('x')).set_delay(Duration::from_secs(5))],
    )
    .await;
    let serial_account = store.account("acct_slot_a").expect("serial account");
    let serial_bundle = Arc::clone(&bundle);
    let serial_id = serial_account.id().clone();
    let serial_target = probe_target("serial", &serial_proxy);
    let running = tokio::spawn(async move {
        serial_bundle
            .admin_provider()
            .probe_turn_state(
                &serial_id,
                &upstream_model("gpt-serial-a"),
                vec![serial_target],
                TurnStateSource::ManualProbe,
            )
            .await
    });
    wait_for_request(&serial_seen, &serial_count, 1).await;
    let conflict = bundle
        .admin_provider()
        .probe_turn_state(
            serial_account.id(),
            &upstream_model("gpt-serial-b"),
            vec![probe_target("serial", &serial_proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect_err("same account manual probe must conflict");
    assert_eq!(conflict.kind(), ProviderAdminErrorKind::Conflict);
    let automatic = bundle
        .admin_provider()
        .probe_turn_state(
            serial_account.id(),
            &upstream_model("gpt-serial-b"),
            vec![probe_target("serial", &serial_proxy)],
            TurnStateSource::AutomaticRenewal,
        )
        .await
        .expect("busy automatic probe");
    assert!(automatic.attempts.is_empty());
    assert_eq!(
        serial_proxy
            .received_requests()
            .await
            .expect("serial requests")
            .len(),
        1
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    running.await.expect("serial task").expect("serial result");
}

#[tokio::test]
async fn turn_state_probe_cooldowns_match_failure_scope_and_cap() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let proxy_count = mount_turn_state_sequence(
        &proxy,
        Arc::clone(&seen),
        vec![
            probe_response(407, None),
            probe_response(407, None),
            probe_response(407, None),
            probe_response(407, None),
            probe_response(200, Some('r')),
            probe_response(407, None),
            probe_response(200, Some('s')),
        ],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_proxy_cooldown"]).await;
    let account = store
        .account("acct_proxy_cooldown")
        .expect("cooldown account");
    let model = upstream_model("gpt-5.4");
    let target = probe_target("cooldown", &proxy);
    let _clock = PausedTimeGuard::new();

    for (round, cooldown) in [300_u64, 600, 1_200, 1_800].into_iter().enumerate() {
        let result = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &model,
                vec![target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect("407 probe result");
        assert_eq!(result.attempts.len(), 1);
        assert_eq!(result.attempts[0].status_code, Some(407));
        let blocked = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &upstream_model("gpt-other-model"),
                vec![target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect_err("account proxy cooldown must cross models");
        assert_eq!(blocked.kind(), ProviderAdminErrorKind::Unavailable);
        assert_eq!(
            proxy
                .received_requests()
                .await
                .expect("cooldown requests")
                .len(),
            round + 1
        );
        assert_eq!(proxy_count.load(Ordering::SeqCst), round + 1);
        tokio::time::advance(Duration::from_secs(cooldown - 1)).await;
        let before_boundary = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &model,
                vec![target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect_err("proxy cooldown remains active one second before expiry");
        assert_eq!(before_boundary.kind(), ProviderAdminErrorKind::Unavailable);
        assert_eq!(proxy_count.load(Ordering::SeqCst), round + 1);
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    let recovered = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![target.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("proxy cooldown recovery");
    assert_eq!(recovered.active_target_id.as_deref(), Some("cooldown"));
    assert_eq!(
        proxy
            .received_requests()
            .await
            .expect("recovery request")
            .len(),
        5
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    let reset_failure = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![target.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("post-success 407");
    assert_eq!(reset_failure.attempts[0].status_code, Some(407));
    tokio::time::advance(Duration::from_secs(299)).await;
    assert!(
        bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &model,
                vec![target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .is_err()
    );
    assert_eq!(proxy_count.load(Ordering::SeqCst), 6);
    tokio::time::advance(Duration::from_secs(1)).await;
    let reset_recovered = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![target],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("proxy success resets cooldown progression");
    assert!(reset_recovered.active_target_id.is_some());
    assert_eq!(proxy_count.load(Ordering::SeqCst), 7);
}

#[tokio::test]
async fn turn_state_probe_model_cooldown_does_not_cross_models() {
    let _clock = PausedTimeGuard::new();
    for status in [312_u16, 200] {
        let base = MockServer::start().await;
        let proxy = MockServer::start().await;
        let seen = Arc::new(Notify::new());
        let request_count = mount_turn_state_sequence(
            &proxy,
            Arc::clone(&seen),
            vec![probe_response(status, None), probe_response(200, Some('m'))],
        )
        .await;
        let account_name = format!("acct_model_cooldown_{status}");
        let (bundle, store) = turn_state_fixture(&base, &[account_name.as_str()]).await;
        let account = store
            .account(&account_name)
            .expect("model cooldown account");
        let target = probe_target("model-scoped", &proxy);
        let failed_model = upstream_model("gpt-model-failed");
        let result = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &failed_model,
                vec![target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect("model failure");
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        assert_eq!(result.attempts[0].status_code, Some(status));
        assert!(bundle.admin_provider().due_turn_state_subjects().is_empty());
        let blocked = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &failed_model,
                vec![target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect_err("same model must be cooled");
        assert_eq!(blocked.kind(), ProviderAdminErrorKind::Unavailable);

        tokio::time::advance(Duration::from_secs(10)).await;
        let other_bundle = Arc::clone(&bundle);
        let other_account = account.id().clone();
        let other_target = target.clone();
        let other = tokio::spawn(async move {
            other_bundle
                .admin_provider()
                .probe_turn_state(
                    &other_account,
                    &upstream_model("gpt-model-other"),
                    vec![other_target],
                    TurnStateSource::ManualProbe,
                )
                .await
        });
        wait_for_request(&seen, &request_count, 2).await;
        let other = other
            .await
            .expect("other model task")
            .expect("other model probe");
        assert_eq!(other.active_target_id.as_deref(), Some("model-scoped"));
        assert_eq!(
            proxy
                .received_requests()
                .await
                .expect("model requests")
                .len(),
            2
        );
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn first_manual_failures_do_not_create_automatic_probe_eligibility() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let request_count = mount_turn_state_sequence(
        &proxy,
        Arc::clone(&seen),
        vec![probe_response(200, None), probe_response(312, None)],
    )
    .await;
    let (bundle, store) =
        turn_state_fixture(&base, &["acct_first_missing", "acct_first_312"]).await;
    let _clock = PausedTimeGuard::new();
    for (account_name, status, model_name) in [
        ("acct_first_missing", 200_u16, "gpt-first-missing"),
        ("acct_first_312", 312_u16, "gpt-first-312"),
    ] {
        let account = store.account(account_name).expect("first failure account");
        let model = upstream_model(model_name);
        let result = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &model,
                vec![probe_target(account_name, &proxy)],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect("first manual failure");
        assert_eq!(result.attempts.len(), 1);
        assert_eq!(result.attempts[0].status_code, Some(status));
        assert!(!result.attempts[0].state_acquired);
        assert!(result.active_target_id.is_none());
        let snapshot = bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &model)
            .expect("first failure snapshot");
        assert_eq!(snapshot.probe_history, vec![result]);
    }
    assert_eq!(request_count.load(Ordering::SeqCst), 2);
    assert!(bundle.admin_provider().due_turn_state_subjects().is_empty());
    tokio::time::advance(Duration::from_secs(1_801)).await;
    assert!(bundle.admin_provider().due_turn_state_subjects().is_empty());
    assert_eq!(request_count.load(Ordering::SeqCst), 2);
    for (account_name, model_name) in [
        ("acct_first_missing", "gpt-first-missing"),
        ("acct_first_312", "gpt-first-312"),
    ] {
        let account = store.account(account_name).expect("first failure account");
        assert_eq!(
            bundle
                .admin_provider()
                .turn_state_snapshot(account.id(), &upstream_model(model_name))
                .expect("unchanged first failure snapshot")
                .probe_history
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn turn_state_probe_accepts_valid_state_on_312_and_short_circuits() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let request_count = mount_turn_state_sequence(
        &proxy,
        Arc::clone(&seen),
        vec![probe_response(312, Some('t'))],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_312_state"]).await;
    let account = store.account("acct_312_state").expect("312 state account");
    let model = upstream_model("gpt-312-state");
    let _clock = PausedTimeGuard::new();
    let result = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            probe_targets(&proxy, &["a", "b"]),
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("312 state result");
    assert_eq!(request_count.load(Ordering::SeqCst), 1);
    assert_eq!(result.attempts.len(), 1);
    assert_eq!(result.attempts[0].status_code, Some(312));
    assert!(result.attempts[0].state_acquired);
    assert_eq!(result.active_target_id.as_deref(), Some("a"));
    assert!(result.state_expires_at.is_some());
    let snapshot = bundle
        .admin_provider()
        .turn_state_snapshot(account.id(), &model)
        .expect("312 state snapshot");
    assert!(snapshot.invalidation_reason.is_none());
    assert_eq!(snapshot.probe_history, vec![result]);
}

#[tokio::test]
async fn turn_state_probe_auth_and_rate_limit_failures_stop_without_switching_proxy() {
    let _clock = PausedTimeGuard::new();
    for status in [401_u16, 403] {
        let base = MockServer::start().await;
        let proxy = MockServer::start().await;
        let seen = Arc::new(Notify::new());
        let _ = mount_turn_state_sequence(
            &proxy,
            Arc::clone(&seen),
            vec![probe_response(status, None), probe_response(200, Some('a'))],
        )
        .await;
        let account_name = format!("acct_auth_{status}");
        let (bundle, store) = turn_state_fixture(&base, &[account_name.as_str()]).await;
        let account = store.account(&account_name).expect("auth account");
        let target = probe_target("auth", &proxy);
        let result = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &upstream_model("gpt-auth-failed"),
                vec![target.clone(), probe_target("unused", &proxy)],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect("auth result");
        assert_eq!(result.attempts.len(), 1);
        assert_eq!(result.attempts[0].status_code, Some(status));
        tokio::time::advance(Duration::from_secs(45)).await;
        assert!(
            bundle
                .admin_provider()
                .probe_turn_state(
                    account.id(),
                    &upstream_model("gpt-auth-failed"),
                    vec![target.clone()],
                    TurnStateSource::ManualProbe,
                )
                .await
                .is_err()
        );
        let other = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &upstream_model("gpt-auth-other"),
                vec![target],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect("auth failures do not cool proxy");
        assert_eq!(other.active_target_id.as_deref(), Some("auth"));
        assert_eq!(
            proxy
                .received_requests()
                .await
                .expect("auth requests")
                .len(),
            2
        );
    }

    let base = MockServer::start().await;
    let first_proxy = MockServer::start().await;
    let unused_proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let rate_count = mount_turn_state_sequence(
        &first_proxy,
        Arc::clone(&seen),
        vec![
            probe_response(429, None),
            probe_response(200, Some('q')),
            probe_response(429, None),
            probe_response(200, Some('z')),
        ],
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(probe_response(200, Some('u')))
        .expect(0)
        .mount(&unused_proxy)
        .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_rate_limited"]).await;
    let account = store.account("acct_rate_limited").expect("rate account");
    let first_target = probe_target("first", &first_proxy);
    let limited = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-rate-a"),
            vec![first_target.clone(), probe_target("unused", &unused_proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("429 result");
    assert_eq!(limited.attempts.len(), 1);
    assert_eq!(limited.attempts[0].status_code, Some(429));
    assert!(
        bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &upstream_model("gpt-rate-b"),
                vec![first_target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .is_err()
    );
    let account_delay = turn_state_backoff("acct_rate_limited", None, 1);
    tokio::time::advance(account_delay - Duration::from_secs(1)).await;
    assert!(
        bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &upstream_model("gpt-rate-b"),
                vec![first_target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .is_err()
    );
    assert_eq!(rate_count.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    let recovered = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-rate-b"),
            vec![first_target.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("429 recovery");
    assert!(recovered.active_target_id.is_some());
    tokio::time::advance(Duration::from_secs(10)).await;
    let limited_again = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-rate-c"),
            vec![first_target.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("second 429 after success");
    assert_eq!(limited_again.attempts[0].status_code, Some(429));
    tokio::time::advance(account_delay - Duration::from_secs(1)).await;
    assert!(
        bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &upstream_model("gpt-rate-d"),
                vec![first_target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .is_err()
    );
    assert_eq!(rate_count.load(Ordering::SeqCst), 3);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::advance(
        Duration::from_secs(300).saturating_sub(account_delay * 2 + Duration::from_secs(10)),
    )
    .await;
    let reset_recovered = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-rate-d"),
            vec![first_target],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("successful probe reset 429 progression");
    assert!(reset_recovered.active_target_id.is_some());
    assert_eq!(
        first_proxy
            .received_requests()
            .await
            .expect("rate requests")
            .len(),
        4
    );
}

#[tokio::test]
async fn turn_state_probe_key_backoff_uses_deterministic_jitter_and_resets_after_success() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let request_count = mount_turn_state_sequence(
        &proxy,
        Arc::clone(&seen),
        vec![
            probe_response(401, None),
            probe_response(401, None),
            probe_response(401, None),
            probe_response(401, None),
            probe_response(401, None),
            probe_response(200, Some('k')),
            probe_response(401, None),
            probe_response(200, Some('r')),
        ],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_key_backoff"]).await;
    let account = store
        .account("acct_key_backoff")
        .expect("key backoff account");
    let model = upstream_model("gpt-key-backoff");
    let target = probe_target("key", &proxy);
    let _clock = PausedTimeGuard::new();
    for failure_count in 1..=5 {
        let failed = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &model,
                vec![target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect("401 probe result");
        assert_eq!(failed.attempts[0].status_code, Some(401));
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            usize::try_from(failure_count).expect("failure count")
        );
        let delay = turn_state_backoff("acct_key_backoff", Some("gpt-key-backoff"), failure_count);
        tokio::time::advance(delay - Duration::from_secs(1)).await;
        let blocked = bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &model,
                vec![target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .expect_err("key backoff remains active before exact boundary");
        assert_eq!(blocked.kind(), ProviderAdminErrorKind::Unavailable);
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            usize::try_from(failure_count).expect("failure count")
        );
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    let recovered = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![target.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("key backoff recovery");
    assert!(recovered.active_target_id.is_some());
    tokio::time::advance(Duration::from_secs(10)).await;
    let failed_after_success = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![target.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("401 after reset");
    assert_eq!(failed_after_success.attempts[0].status_code, Some(401));
    let reset_delay = turn_state_backoff("acct_key_backoff", Some("gpt-key-backoff"), 1);
    tokio::time::advance(reset_delay - Duration::from_secs(1)).await;
    assert!(
        bundle
            .admin_provider()
            .probe_turn_state(
                account.id(),
                &model,
                vec![target.clone()],
                TurnStateSource::ManualProbe,
            )
            .await
            .is_err()
    );
    assert_eq!(request_count.load(Ordering::SeqCst), 7);
    tokio::time::advance(Duration::from_secs(1)).await;
    let reset_recovered = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![target],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("first backoff restored after success");
    assert!(reset_recovered.active_target_id.is_some());
    assert_eq!(request_count.load(Ordering::SeqCst), 8);
}

#[tokio::test]
async fn turn_state_probe_timeout_cools_account_proxy_without_holding_global_slot() {
    let base = MockServer::start().await;
    let slow_proxy = MockServer::start().await;
    let fast_proxy = MockServer::start().await;
    let slow_seen = Arc::new(Notify::new());
    let fast_seen = Arc::new(Notify::new());
    let slow_count = mount_turn_state_sequence(
        &slow_proxy,
        Arc::clone(&slow_seen),
        vec![probe_response(200, Some('t')).set_delay(Duration::from_secs(31))],
    )
    .await;
    let fast_count = mount_turn_state_sequence(
        &fast_proxy,
        Arc::clone(&fast_seen),
        vec![
            probe_response(200, Some('f')).set_delay(Duration::from_secs(5)),
            probe_response(200, Some('g')).set_delay(Duration::from_secs(5)),
        ],
    )
    .await;
    let (bundle, store) =
        turn_state_fixture(&base, &["acct_timeout", "acct_fast_a", "acct_fast_b"]).await;
    let _clock = PausedTimeGuard::new();
    let timeout_bundle = Arc::clone(&bundle);
    let timeout_account = store
        .account("acct_timeout")
        .expect("timeout account")
        .id()
        .clone();
    let slow_target = probe_target("slow", &slow_proxy);
    let slow_target_for_task = slow_target.clone();
    let timeout = tokio::spawn(async move {
        timeout_bundle
            .admin_provider()
            .probe_turn_state(
                &timeout_account,
                &upstream_model("gpt-timeout"),
                vec![slow_target_for_task],
                TurnStateSource::ManualProbe,
            )
            .await
    });
    wait_for_request(&slow_seen, &slow_count, 1).await;

    let mut fast_tasks = Vec::new();
    for account_name in ["acct_fast_a", "acct_fast_b"] {
        let fast_bundle = Arc::clone(&bundle);
        let fast_account = store
            .account(account_name)
            .expect("fast account")
            .id()
            .clone();
        let target = probe_target(account_name, &fast_proxy);
        fast_tasks.push(tokio::spawn(async move {
            fast_bundle
                .admin_provider()
                .probe_turn_state(
                    &fast_account,
                    &upstream_model("gpt-fast"),
                    vec![target],
                    TurnStateSource::ManualProbe,
                )
                .await
        }));
    }
    wait_for_request(&fast_seen, &fast_count, 1).await;
    assert_eq!(
        fast_proxy
            .received_requests()
            .await
            .expect("one free slot")
            .len(),
        1
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    wait_for_request(&fast_seen, &fast_count, 2).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    for task in fast_tasks {
        assert!(
            task.await
                .expect("fast task")
                .expect("fast result")
                .active_target_id
                .is_some()
        );
    }
    // 越过定时器边界，避免毫秒取整让恰好 30 秒的虚拟时钟尚未唤醒超时。
    tokio::time::advance(Duration::from_secs(21)).await;
    let timeout = timeout
        .await
        .expect("timeout task")
        .expect("timeout result");
    assert_eq!(timeout.attempts.len(), 1);
    assert!(timeout.attempts[0].status_code.is_none());
    assert_eq!(
        fast_proxy
            .received_requests()
            .await
            .expect("fast requests")
            .len(),
        2
    );
    let blocked = bundle
        .admin_provider()
        .probe_turn_state(
            store.account("acct_timeout").expect("timeout account").id(),
            &upstream_model("gpt-timeout-other"),
            vec![slow_target],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect_err("timeout cooldown is account proxy scoped");
    assert_eq!(blocked.kind(), ProviderAdminErrorKind::Unavailable);
    assert_eq!(slow_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cooled_account_does_not_occupy_global_probe_slots() {
    let base = MockServer::start().await;
    let cooled_proxy = MockServer::start().await;
    let active_proxy = MockServer::start().await;
    let cooled_seen = Arc::new(Notify::new());
    let active_seen = Arc::new(Notify::new());
    let cooled_count = mount_turn_state_sequence(
        &cooled_proxy,
        Arc::clone(&cooled_seen),
        vec![probe_response(407, None)],
    )
    .await;
    let active_count = mount_turn_state_sequence(
        &active_proxy,
        Arc::clone(&active_seen),
        vec![
            probe_response(200, Some('a')).set_delay(Duration::from_secs(5)),
            probe_response(200, Some('b')).set_delay(Duration::from_secs(5)),
        ],
    )
    .await;
    let (bundle, store) =
        turn_state_fixture(&base, &["acct_cooled", "acct_active_a", "acct_active_b"]).await;
    let _clock = PausedTimeGuard::new();
    let cooled_account = store.account("acct_cooled").expect("cooled account");
    let cooled_target = probe_target("cooled", &cooled_proxy);
    let failed = bundle
        .admin_provider()
        .probe_turn_state(
            cooled_account.id(),
            &upstream_model("gpt-cooled"),
            vec![cooled_target.clone()],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("407 result");
    assert_eq!(failed.attempts[0].status_code, Some(407));
    tokio::time::advance(Duration::from_secs(10)).await;
    let blocked = bundle
        .admin_provider()
        .probe_turn_state(
            cooled_account.id(),
            &upstream_model("gpt-cooled-other"),
            vec![cooled_target],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect_err("cooled account rejected before slot acquisition");
    assert_eq!(blocked.kind(), ProviderAdminErrorKind::Unavailable);
    assert_eq!(cooled_count.load(Ordering::SeqCst), 1);

    let mut tasks = Vec::new();
    for account_name in ["acct_active_a", "acct_active_b"] {
        let task_bundle = Arc::clone(&bundle);
        let account_id = store
            .account(account_name)
            .expect("active account")
            .id()
            .clone();
        let target = probe_target(account_name, &active_proxy);
        tasks.push(tokio::spawn(async move {
            task_bundle
                .admin_provider()
                .probe_turn_state(
                    &account_id,
                    &upstream_model("gpt-active"),
                    vec![target],
                    TurnStateSource::ManualProbe,
                )
                .await
        }));
    }
    wait_for_request(&active_seen, &active_count, 1).await;
    wait_for_request(&active_seen, &active_count, 2).await;
    assert_eq!(active_count.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(5)).await;
    for task in tasks {
        assert!(
            task.await
                .expect("active task")
                .expect("active probe")
                .active_target_id
                .is_some()
        );
    }
}

#[tokio::test]
async fn turn_state_probe_network_failure_cools_the_account_proxy() {
    let base = MockServer::start().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("broken proxy listener");
    let address = listener.local_addr().expect("broken proxy address");
    let accepted = Arc::new(Notify::new());
    let accepted_for_task = Arc::clone(&accepted);
    let accept_count = Arc::new(AtomicUsize::new(0));
    let accept_count_for_task = Arc::clone(&accept_count);
    let accept_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept probe connection");
        accept_count_for_task.fetch_add(1, Ordering::SeqCst);
        accepted_for_task.notify_one();
        drop(stream);
    });
    let (bundle, store) = turn_state_fixture(&base, &["acct_network_failure"]).await;
    let account = store
        .account("acct_network_failure")
        .expect("network account");
    let target = TurnStateProbeTarget {
        id: "broken".to_owned(),
        label: "断开连接的本地出口".to_owned(),
        proxy: OutboundProxy::parse(&format!("http://{address}")).expect("broken localhost proxy"),
    };
    let _clock = PausedTimeGuard::new();
    let probe_bundle = Arc::clone(&bundle);
    let probe_account = account.id().clone();
    let probe_target = target.clone();
    let probe = tokio::spawn(async move {
        probe_bundle
            .admin_provider()
            .probe_turn_state(
                &probe_account,
                &upstream_model("gpt-network"),
                vec![probe_target],
                TurnStateSource::ManualProbe,
            )
            .await
    });
    accepted.notified().await;
    accept_task.await.expect("broken proxy task");
    let result = probe
        .await
        .expect("network probe task")
        .expect("network probe");
    assert_eq!(result.attempts.len(), 1);
    assert!(result.attempts[0].status_code.is_none());
    assert_eq!(accept_count.load(Ordering::SeqCst), 1);
    let blocked = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-network-other"),
            vec![target],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect_err("network cooldown is account proxy scoped");
    assert_eq!(blocked.kind(), ProviderAdminErrorKind::Unavailable);
    assert_eq!(accept_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn automatic_turn_state_probe_stops_when_business_refreshes_state() {
    let base = MockServer::start().await;
    let business_index = Arc::new(AtomicUsize::new(0));
    let business_index_for_mock = Arc::clone(&business_index);
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(move |_request: &wiremock::Request| {
            if business_index_for_mock.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(312).set_body_json(json!({"error": "stale"}))
            } else {
                ResponseTemplate::new(200)
                    .insert_header("x-codex-turn-state", "f".repeat(292))
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(COMPLETED_SESSION_SSE)
            }
        })
        .expect(2)
        .mount(&base)
        .await;
    let proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let probe_count = mount_turn_state_sequence(
        &proxy,
        Arc::clone(&seen),
        vec![
            probe_response(200, Some('i')),
            probe_response(200, None).set_delay(Duration::from_secs(5)),
        ],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_auto_fresh"]).await;
    let account = store.account("acct_auto_fresh").expect("automatic account");
    let model = upstream_model("gpt-5.4");
    let targets = probe_targets(&proxy, &["a", "b"]);
    let _clock = PausedTimeGuard::new();
    bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            targets.clone(),
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("qualifying manual probe");
    assert_eq!(probe_count.load(Ordering::SeqCst), 1);
    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        "gpt-5.4",
        "req_auto_invalidate",
        false,
    )
    .await;
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(bundle.admin_provider().due_turn_state_subjects().len(), 1);

    store.set_egress(
        account.id().as_str(),
        Some(OutboundProxy::parse(&proxy.uri()).expect("bound proxy")),
        None,
    );
    let automatic_bundle = Arc::clone(&bundle);
    let automatic_account = account.id().clone();
    let automatic_model = model.clone();
    let automatic = tokio::spawn(async move {
        automatic_bundle
            .admin_provider()
            .probe_turn_state(
                &automatic_account,
                &automatic_model,
                targets,
                TurnStateSource::AutomaticRenewal,
            )
            .await
    });
    wait_for_request(&seen, &probe_count, 2).await;
    store.set_egress(account.id().as_str(), None, None);
    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        "gpt-5.4",
        "req_auto_refresh",
        true,
    )
    .await;
    let refreshed = bundle
        .admin_provider()
        .turn_state_snapshot(account.id(), &model)
        .expect("business refreshed snapshot");
    assert_eq!(
        refreshed.state_source,
        Some(TurnStateSource::UpstreamResponse)
    );
    assert!(refreshed.invalidation_reason.is_none());
    assert!(refreshed.state_expires_at.is_some());
    tokio::time::advance(Duration::from_secs(5)).await;
    let automatic = automatic
        .await
        .expect("automatic task")
        .expect("automatic result");
    assert_eq!(automatic.attempts.len(), 1);
    assert!(automatic.active_target_id.is_none());
    assert!(automatic.state_expires_at.is_none());
    assert_eq!(probe_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        proxy
            .received_requests()
            .await
            .expect("automatic requests")
            .len(),
        2
    );
    let snapshot = bundle
        .admin_provider()
        .turn_state_snapshot(account.id(), &model)
        .expect("fresh business snapshot");
    assert_eq!(
        snapshot.state_source,
        Some(TurnStateSource::UpstreamResponse)
    );
    assert!(snapshot.invalidation_reason.is_none());
    assert!(snapshot.state_expires_at.is_some());
    assert_eq!(snapshot.probe_history[0], automatic);
    assert!(snapshot.probe_history[0].active_target_id.is_none());
    assert!(snapshot.probe_history[0].state_expires_at.is_none());
}

#[tokio::test]
async fn turn_state_probe_stale_results_cannot_replace_newer_business_state() {
    let _clock = PausedTimeGuard::new();
    for stale_status in [200_u16, 312] {
        let base = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/codex/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-codex-turn-state", "n".repeat(292))
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(COMPLETED_SESSION_SSE),
            )
            .expect(1)
            .mount(&base)
            .await;
        let proxy = MockServer::start().await;
        let seen = Arc::new(Notify::new());
        let probe_count = mount_turn_state_sequence(
            &proxy,
            Arc::clone(&seen),
            vec![
                probe_response(stale_status, (stale_status == 200).then_some('o'))
                    .set_delay(Duration::from_secs(5)),
            ],
        )
        .await;
        let account_name = format!("acct_stale_{stale_status}");
        let (bundle, store) = turn_state_fixture(&base, &[account_name.as_str()]).await;
        let account = store.account(&account_name).expect("stale account");
        let probe_bundle = Arc::clone(&bundle);
        let probe_account = account.id().clone();
        let probe_model = upstream_model("gpt-5.4");
        let probe_model_task = probe_model.clone();
        let probe = tokio::spawn(async move {
            probe_bundle
                .admin_provider()
                .probe_turn_state(
                    &probe_account,
                    &probe_model_task,
                    vec![probe_target("stale", &proxy)],
                    TurnStateSource::ManualProbe,
                )
                .await
        });
        wait_for_request(&seen, &probe_count, 1).await;
        drain_turn_state_business(
            &bundle,
            account.id().as_str(),
            "gpt-5.4",
            &format!("req_fresh_{stale_status}"),
            true,
        )
        .await;
        let refreshed = bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &probe_model)
            .expect("business state before stale response");
        assert_eq!(
            refreshed.state_source,
            Some(TurnStateSource::UpstreamResponse)
        );
        assert!(refreshed.invalidation_reason.is_none());
        assert!(refreshed.state_expires_at.is_some());
        tokio::time::advance(Duration::from_secs(5)).await;
        let stale = probe.await.expect("stale task").expect("stale result");
        assert_eq!(stale.attempts.len(), 1);
        assert_eq!(stale.attempts[0].state_acquired, stale_status == 200);
        assert!(stale.active_target_id.is_none());
        assert!(stale.state_expires_at.is_none());
        assert_eq!(probe_count.load(Ordering::SeqCst), 1);
        let snapshot = bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &probe_model)
            .expect("fresh snapshot");
        assert_eq!(
            snapshot.state_source,
            Some(TurnStateSource::UpstreamResponse)
        );
        assert!(snapshot.invalidation_reason.is_none());
        assert_eq!(snapshot.probe_history[0], stale);
        assert!(snapshot.probe_history[0].active_target_id.is_none());
        assert!(snapshot.probe_history[0].state_expires_at.is_none());
    }
}

#[tokio::test]
async fn turn_state_probe_exit_change_keeps_only_diagnostics() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let seen = Arc::new(Notify::new());
    let count = mount_turn_state_sequence(
        &proxy,
        Arc::clone(&seen),
        vec![probe_response(200, Some('s')).set_delay(Duration::from_secs(5))],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_exit_change"]).await;
    let account = store.account("acct_exit_change").expect("account");
    let account_id = account.id().clone();
    let probing_bundle = Arc::clone(&bundle);
    let _clock = PausedTimeGuard::new();
    let probing = tokio::spawn(async move {
        probing_bundle
            .admin_provider()
            .probe_turn_state(
                &account_id,
                &upstream_model("gpt-5.4"),
                vec![probe_target("old", &proxy)],
                TurnStateSource::ManualProbe,
            )
            .await
    });
    wait_for_request(&seen, &count, 1).await;
    store.set_egress(
        account.id().as_str(),
        Some(OutboundProxy::parse(&base.uri()).expect("new exit")),
        None,
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    let result = probing.await.expect("task").expect("result");
    assert_eq!(result.attempts.len(), 1);
    assert!(result.attempts[0].state_acquired);
    assert!(result.active_target_id.is_none());
    let snapshot = bundle
        .admin_provider()
        .turn_state_snapshot(account.id(), &upstream_model("gpt-5.4"))
        .expect("snapshot");
    assert!(snapshot.state_expires_at.is_none());
    assert_eq!(snapshot.probe_history, vec![result]);
}

#[tokio::test]
async fn turn_state_snapshot_and_late_application_do_not_mark_rotated_state() {
    let base = MockServer::start().await;
    let response_index = Arc::new(AtomicUsize::new(0));
    let business_seen = Arc::new(Notify::new());
    let response_index_for_mock = Arc::clone(&response_index);
    let business_seen_for_mock = Arc::clone(&business_seen);
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(move |_request: &wiremock::Request| {
            let index = response_index_for_mock.fetch_add(1, Ordering::SeqCst);
            business_seen_for_mock.notify_one();
            let template = ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(COMPLETED_SESSION_SSE);
            if index == 0 {
                template.set_delay(Duration::from_secs(5))
            } else {
                template.insert_header("x-codex-turn-state", "n".repeat(292))
            }
        })
        .expect(2)
        .mount(&base)
        .await;
    let proxy = MockServer::start().await;
    let probe_seen = Arc::new(Notify::new());
    let _probe_count = mount_turn_state_sequence(
        &proxy,
        Arc::clone(&probe_seen),
        vec![probe_response(200, Some('o'))],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_late_apply"]).await;
    let account = store
        .account("acct_late_apply")
        .expect("late apply account");
    let model = upstream_model("gpt-5.4");
    let _clock = PausedTimeGuard::new();
    bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![probe_target("old", &proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("seed old state");
    let read_only = bundle
        .admin_provider()
        .turn_state_snapshot(account.id(), &model)
        .expect("read snapshot");
    assert!(read_only.state_first_applied_at.is_none());

    let old_bundle = Arc::clone(&bundle);
    let old_account = account.id().as_str().to_owned();
    let old_request = tokio::spawn(async move {
        drain_turn_state_business(
            &old_bundle,
            &old_account,
            "gpt-5.4",
            "req_apply_old_late",
            true,
        )
        .await;
    });
    wait_for_request(&business_seen, &response_index, 1).await;
    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        "gpt-5.4",
        "req_rotate_new",
        true,
    )
    .await;
    assert_eq!(response_index.load(Ordering::SeqCst), 2);
    let rotated = bundle
        .admin_provider()
        .turn_state_snapshot(account.id(), &model)
        .expect("rotated snapshot");
    assert_eq!(
        rotated.state_source,
        Some(TurnStateSource::UpstreamResponse)
    );
    assert!(rotated.state_first_applied_at.is_none());
    tokio::time::advance(Duration::from_secs(5)).await;
    old_request.await.expect("old business task");
    assert_eq!(response_index.load(Ordering::SeqCst), 2);
    assert!(
        bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &model)
            .expect("late application snapshot")
            .state_first_applied_at
            .is_none()
    );
    let requests = base.received_requests().await.expect("business history");
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        request
            .headers
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "o".repeat(292))
    }));
}

#[tokio::test]
async fn turn_state_proxy_address_change_clears_old_cooldown() {
    let base = MockServer::start().await;
    let old_proxy = MockServer::start().await;
    let new_proxy = MockServer::start().await;
    let old_seen = Arc::new(Notify::new());
    let new_seen = Arc::new(Notify::new());
    let old_count = mount_turn_state_sequence(
        &old_proxy,
        Arc::clone(&old_seen),
        vec![probe_response(407, None)],
    )
    .await;
    let new_count = mount_turn_state_sequence(
        &new_proxy,
        Arc::clone(&new_seen),
        vec![probe_response(200, Some('n'))],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_proxy_address_change"]).await;
    let account = store
        .account("acct_proxy_address_change")
        .expect("proxy address account");
    let _clock = PausedTimeGuard::new();
    let failed = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-address-old"),
            vec![probe_target("b", &old_proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("old address 407");
    assert_eq!(failed.attempts[0].status_code, Some(407));
    assert_eq!(old_count.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(10)).await;
    let recovered = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-address-new"),
            vec![probe_target("b", &new_proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("same ID with new address is not cooled");
    assert_eq!(recovered.active_target_id.as_deref(), Some("b"));
    assert_eq!(old_count.load(Ordering::SeqCst), 1);
    assert_eq!(new_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn turn_state_proxy_address_change_clears_old_preference() {
    let base = MockServer::start().await;
    let proxy_a = MockServer::start().await;
    let old_b = MockServer::start().await;
    let new_b = MockServer::start().await;
    let a_seen = Arc::new(Notify::new());
    let old_b_seen = Arc::new(Notify::new());
    let a_count = mount_turn_state_sequence(
        &proxy_a,
        Arc::clone(&a_seen),
        vec![probe_response(200, Some('a'))],
    )
    .await;
    let old_b_count = mount_turn_state_sequence(
        &old_b,
        Arc::clone(&old_b_seen),
        vec![probe_response(200, Some('b'))],
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(probe_response(200, Some('n')))
        .expect(0)
        .mount(&new_b)
        .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_proxy_preference_change"]).await;
    let account = store
        .account("acct_proxy_preference_change")
        .expect("proxy preference account");
    let model = upstream_model("gpt-proxy-preference");
    let _clock = PausedTimeGuard::new();
    let seed = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![probe_target("b", &old_b)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("seed b preference");
    assert_eq!(seed.active_target_id.as_deref(), Some("b"));
    tokio::time::advance(Duration::from_secs(10)).await;
    let changed = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![probe_target("a", &proxy_a), probe_target("b", &new_b)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("changed preference candidates");
    assert_eq!(changed.active_target_id.as_deref(), Some("a"));
    assert_eq!(changed.attempts.len(), 1);
    assert_eq!(a_count.load(Ordering::SeqCst), 1);
    assert_eq!(old_b_count.load(Ordering::SeqCst), 1);
    assert!(
        new_b
            .received_requests()
            .await
            .expect("new b requests")
            .is_empty()
    );
}

#[tokio::test]
async fn removing_candidate_clears_its_old_cooldown() {
    let base = MockServer::start().await;
    let proxy_a = MockServer::start().await;
    let proxy_b = MockServer::start().await;
    let a_seen = Arc::new(Notify::new());
    let b_seen = Arc::new(Notify::new());
    let a_count = mount_turn_state_sequence(
        &proxy_a,
        Arc::clone(&a_seen),
        vec![probe_response(401, None)],
    )
    .await;
    let b_count = mount_turn_state_sequence(
        &proxy_b,
        Arc::clone(&b_seen),
        vec![probe_response(407, None), probe_response(200, Some('b'))],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_candidate_cooldown_delete"]).await;
    let account = store
        .account("acct_candidate_cooldown_delete")
        .expect("candidate cooldown account");
    let _clock = PausedTimeGuard::new();
    let first = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-delete-cooldown-1"),
            vec![probe_target("b", &proxy_b)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("seed b cooldown");
    assert_eq!(first.attempts[0].status_code, Some(407));
    tokio::time::advance(Duration::from_secs(10)).await;
    let second = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-delete-cooldown-2"),
            vec![probe_target("a", &proxy_a)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("remove b from directory");
    assert_eq!(second.attempts[0].status_code, Some(401));
    tokio::time::advance(Duration::from_secs(10)).await;
    let third = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-delete-cooldown-3"),
            vec![probe_target("b", &proxy_b)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("restored b has no old cooldown");
    assert_eq!(third.active_target_id.as_deref(), Some("b"));
    assert_eq!(a_count.load(Ordering::SeqCst), 1);
    assert_eq!(b_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn removing_candidate_clears_its_old_preference() {
    let base = MockServer::start().await;
    let proxy_a = MockServer::start().await;
    let proxy_b = MockServer::start().await;
    let a_seen = Arc::new(Notify::new());
    let b_seen = Arc::new(Notify::new());
    let a_count = mount_turn_state_sequence(
        &proxy_a,
        Arc::clone(&a_seen),
        vec![probe_response(401, None), probe_response(200, Some('a'))],
    )
    .await;
    let b_count = mount_turn_state_sequence(
        &proxy_b,
        Arc::clone(&b_seen),
        vec![probe_response(200, Some('b'))],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_candidate_preference_delete"]).await;
    let account = store
        .account("acct_candidate_preference_delete")
        .expect("candidate preference account");
    let _clock = PausedTimeGuard::new();
    let first = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-delete-preference-1"),
            vec![probe_target("b", &proxy_b)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("seed b account preference");
    assert_eq!(first.active_target_id.as_deref(), Some("b"));
    tokio::time::advance(Duration::from_secs(10)).await;
    let second = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-delete-preference-2"),
            vec![probe_target("a", &proxy_a)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("remove b preference");
    assert_eq!(second.attempts[0].status_code, Some(401));
    tokio::time::advance(Duration::from_secs(10)).await;
    let third = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-delete-preference-3"),
            vec![probe_target("a", &proxy_a), probe_target("b", &proxy_b)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("deleted b preference does not revive");
    assert_eq!(third.active_target_id.as_deref(), Some("a"));
    assert_eq!(third.attempts.len(), 1);
    assert_eq!(a_count.load(Ordering::SeqCst), 2);
    assert_eq!(b_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn account_removal_during_probe_discards_natural_completion_and_preserves_other_account() {
    let base = MockServer::start().await;
    let old_proxy = MockServer::start().await;
    let new_proxy = MockServer::start().await;
    let other_proxy = MockServer::start().await;
    let old_seen = Arc::new(Notify::new());
    let new_seen = Arc::new(Notify::new());
    let other_seen = Arc::new(Notify::new());
    let old_count = mount_turn_state_sequence(
        &old_proxy,
        Arc::clone(&old_seen),
        vec![probe_response(200, Some('o')).set_delay(Duration::from_secs(5))],
    )
    .await;
    let new_count = mount_turn_state_sequence(
        &new_proxy,
        Arc::clone(&new_seen),
        vec![probe_response(200, Some('n'))],
    )
    .await;
    let other_count = mount_turn_state_sequence(
        &other_proxy,
        Arc::clone(&other_seen),
        vec![probe_response(200, Some('x'))],
    )
    .await;
    let (bundle, store) =
        turn_state_fixture(&base, &["acct_natural_remove", "acct_natural_other"]).await;
    let account = store
        .account("acct_natural_remove")
        .expect("natural removal account");
    let other_account = store
        .account("acct_natural_other")
        .expect("isolated account");
    let old_model = upstream_model("gpt-natural-old");
    let new_model = upstream_model("gpt-natural-new");
    let other_model = upstream_model("gpt-natural-other");
    let _clock = PausedTimeGuard::new();
    bundle
        .admin_provider()
        .probe_turn_state(
            other_account.id(),
            &other_model,
            vec![probe_target("other", &other_proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("seed isolated account");
    assert_eq!(other_count.load(Ordering::SeqCst), 1);

    let running_bundle = Arc::clone(&bundle);
    let running_account = account.id().clone();
    let running_model = old_model.clone();
    let old_target = probe_target("old", &old_proxy);
    let running = tokio::spawn(async move {
        running_bundle
            .admin_provider()
            .probe_turn_state(
                &running_account,
                &running_model,
                vec![old_target],
                TurnStateSource::ManualProbe,
            )
            .await
    });
    wait_for_request(&old_seen, &old_count, 1).await;
    bundle
        .admin_provider()
        .account_unavailable(account.id())
        .await;
    let rejected = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &new_model,
            vec![probe_target("new", &new_proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect_err("old run keeps the removed account gate");
    assert!(matches!(
        rejected.kind(),
        ProviderAdminErrorKind::Conflict | ProviderAdminErrorKind::Unavailable
    ));
    assert_eq!(new_count.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(5)).await;
    let old_result = running
        .await
        .expect("natural old task")
        .expect("natural old result");
    assert!(old_result.active_target_id.is_none());
    assert!(old_result.state_expires_at.is_none());
    assert!(
        bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &old_model)
            .is_none()
    );
    assert!(
        bundle
            .admin_provider()
            .turn_state_overview()
            .iter()
            .all(|entry| entry.account_id != account.id().as_str())
    );
    assert!(
        bundle
            .admin_provider()
            .turn_state_snapshot(other_account.id(), &other_model)
            .is_some()
    );

    tokio::time::advance(Duration::from_secs(4)).await;
    let replacement_bundle = Arc::clone(&bundle);
    let replacement_account = account.id().clone();
    let replacement_model = new_model.clone();
    let new_target = probe_target("new", &new_proxy);
    let replacement = tokio::spawn(async move {
        replacement_bundle
            .admin_provider()
            .probe_turn_state(
                &replacement_account,
                &replacement_model,
                vec![new_target],
                TurnStateSource::ManualProbe,
            )
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(new_count.load(Ordering::SeqCst), 0);
    assert!(!replacement.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for_request(&new_seen, &new_count, 1).await;
    let new_result = replacement
        .await
        .expect("replacement task")
        .expect("replacement result");
    assert_eq!(new_result.active_target_id.as_deref(), Some("new"));
    assert_eq!(old_count.load(Ordering::SeqCst), 1);
    assert_eq!(new_count.load(Ordering::SeqCst), 1);
    assert!(
        bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &old_model)
            .is_none()
    );
    let new_snapshot = bundle
        .admin_provider()
        .turn_state_snapshot(account.id(), &new_model)
        .expect("replacement snapshot");
    assert_eq!(new_snapshot.probe_history, vec![new_result]);
    let account_overview = bundle
        .admin_provider()
        .turn_state_overview()
        .into_iter()
        .filter(|entry| entry.account_id == account.id().as_str())
        .collect::<Vec<_>>();
    assert_eq!(account_overview.len(), 1);
    assert_eq!(account_overview[0].model, new_model.as_str());
}

#[tokio::test]
async fn turn_state_probe_cancellation_and_account_removal_release_the_original_gate() {
    let base = MockServer::start().await;
    let old_proxy = MockServer::start().await;
    let new_proxy = MockServer::start().await;
    let old_seen = Arc::new(Notify::new());
    let new_seen = Arc::new(Notify::new());
    let old_count = mount_turn_state_sequence(
        &old_proxy,
        Arc::clone(&old_seen),
        vec![probe_response(200, Some('o')).set_delay(Duration::from_secs(60))],
    )
    .await;
    let new_count = mount_turn_state_sequence(
        &new_proxy,
        Arc::clone(&new_seen),
        vec![probe_response(200, Some('n'))],
    )
    .await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_cancel_probe"]).await;
    let account = store.account("acct_cancel_probe").expect("cancel account");
    let _clock = PausedTimeGuard::new();
    let running_bundle = Arc::clone(&bundle);
    let running_account = account.id().clone();
    let old_target = probe_target("old", &old_proxy);
    let running = tokio::spawn(async move {
        running_bundle
            .admin_provider()
            .probe_turn_state(
                &running_account,
                &upstream_model("gpt-cancel-old"),
                vec![old_target],
                TurnStateSource::ManualProbe,
            )
            .await
    });
    wait_for_request(&old_seen, &old_count, 1).await;
    bundle
        .admin_provider()
        .account_unavailable(account.id())
        .await;
    let while_old_running = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-cancel-new"),
            vec![probe_target("new", &new_proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect_err("removed account cannot create a second gate");
    assert!(matches!(
        while_old_running.kind(),
        ProviderAdminErrorKind::Conflict | ProviderAdminErrorKind::Unavailable
    ));
    assert!(
        new_proxy
            .received_requests()
            .await
            .expect("new requests")
            .is_empty()
    );
    assert_eq!(new_count.load(Ordering::SeqCst), 0);
    running.abort();
    let _ = running.await;
    tokio::time::advance(Duration::from_secs(10)).await;
    let replacement = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-cancel-new"),
            vec![probe_target("new", &new_proxy)],
            TurnStateSource::ManualProbe,
        )
        .await
        .expect("cancelled gate released");
    assert_eq!(replacement.active_target_id.as_deref(), Some("new"));
    assert_eq!(
        old_proxy
            .received_requests()
            .await
            .expect("old requests")
            .len(),
        1
    );
    assert_eq!(
        new_proxy
            .received_requests()
            .await
            .expect("new requests")
            .len(),
        1
    );
    assert_eq!(old_count.load(Ordering::SeqCst), 1);
    assert_eq!(new_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn openai_admin_quota_refresh_updates_the_account_plan() {
    let store = Arc::new(MemoryAccountStore::default());
    let mut verified_account = profile("chatgpt-upgraded-plan");
    verified_account.plan_type = Some("plus".to_owned());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_upgraded_plan".to_owned(),
            name: "upgraded plan".to_owned(),
            secret: secret("upgraded-plan-test-token"),
            verified_account,
            next_refresh_at: None,
            enabled: true,
        })
        .await;
    let account = store.account("acct_upgraded_plan").unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_type": "pro", "rate_limit": {"allowed": true, "primary_window": {"used_percent": 1}}
        })))
        .expect(1).mount(&server).await;
    let mut config = valid_config();
    config.config.api.base_url = server.uri();
    let bundle = provider_openai::initialize(
        config.config,
        provider_ports_with(store.clone(), Arc::new(TestOAuthPending::default())),
    )
    .await
    .unwrap();
    for refresh in [true, false] {
        let quota = bundle
            .admin_provider()
            .quota(ProviderQuotaRequest {
                account_id: account.id().clone(),
                refresh,
                rolling_usage: None,
            })
            .await
            .unwrap();
        assert_eq!(quota.plan_type.as_deref(), Some("pro"));
        assert_eq!(
            store.account("acct_upgraded_plan").unwrap().plan_type(),
            Some("pro")
        );
    }
}

#[tokio::test]
async fn openai_admin_projects_free_plan_from_cached_quota_when_account_claims_omit_it() {
    let store = Arc::new(MemoryAccountStore::default());
    let mut verified_account = profile("chatgpt-free-plan");
    verified_account.plan_type = None;
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_free_plan".to_owned(),
            name: "free plan".to_owned(),
            secret: secret("free-plan-test-token"),
            verified_account,
            next_refresh_at: None,
            enabled: true,
        })
        .await;
    let account = store.account("acct_free_plan").expect("stored account");
    let observed_at = SystemTime::now();
    store
        .compare_and_swap_quota(QuotaObservation {
            plan_type: None,
            account_id: account.id().clone(),
            expected_revision: account.revision(),
            quota: OpaqueProviderData::new(
                json!({
                    "plan_type": "free",
                    "rate_limit": {
                        "allowed": true,
                        "primary_window": {
                            "used_percent": 0,
                            "limit_window_seconds": 2_592_000,
                            "reset_at": 1_900_000_000
                        }
                    }
                })
                .as_object()
                .expect("quota object")
                .clone(),
            ),
            observed_at,
            state: QuotaState::allowed(observed_at),
        })
        .await
        .expect("persist existing quota");
    let config = valid_config();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(store, Arc::new(TestOAuthPending::default())),
    )
    .await
    .expect("OpenAI bundle");
    let admin = bundle.admin_provider();
    let quota = admin
        .quota(ProviderQuotaRequest {
            account_id: account.id().clone(),
            refresh: false,
            rolling_usage: None,
        })
        .await
        .expect("read cached free quota");
    assert_eq!(quota.plan_type.as_deref(), Some("free"));
    assert_eq!(
        admin.plan_type_display(quota.plan_type.as_deref().expect("plan")),
        "Free"
    );
}

#[tokio::test]
async fn openai_admin_provider_projects_official_codex_quota_and_independent_buckets_with_chinese_labels()
 {
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_admin_canonical_quota".to_owned(),
            name: "admin canonical quota".to_owned(),
            secret: secret("admin-canonical-quota-access"),
            verified_account: profile("chatgpt-admin-canonical-quota"),
            next_refresh_at: Some(chrono::Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    let account = store
        .account("acct_admin_canonical_quota")
        .expect("stored account");
    let raw = json!({
        "active_limit": "premium",
        "rate_limit": {
            "primary_window": {
                "used_percent": 91,
                "reset_at": 1_900_000_000,
                "limit_window_seconds": 2_592_000
            },
            "secondary_window": {
                "used_percent": 88
            }
        },
        "additional_rate_limits": [{
            "limit_name": "custom_codex_label",
            "metered_feature": "codex",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 2,
                    "reset_at": 1_900_000_000,
                    "limit_window_seconds": 2_592_000
                },
                "secondary_window": {
                    "used_percent": 0
                }
            }
        }, {
            "limit_name": "code_review",
            "metered_feature": "code_review",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 12,
                    "reset_at": 1_900_000_000,
                    "limit_window_seconds": 604_800
                }
            }
        }, {
            "limit_name": "GPT-5.3-Codex-Spark",
            "metered_feature": "codex_bengalfox",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 0,
                    "reset_at": 1_900_000_000,
                    "limit_window_seconds": 604_800
                }
            }
        }]
    });
    let observed_at = SystemTime::now();
    store
        .compare_and_swap_quota(QuotaObservation {
            plan_type: None,
            account_id: account.id().clone(),
            expected_revision: account.revision(),
            quota: OpaqueProviderData::new(raw.as_object().expect("quota object").clone()),
            observed_at,
            state: QuotaState::observed_unknown(observed_at),
        })
        .await
        .expect("persist quota");
    let config = valid_config();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(Arc::clone(&store), Arc::new(TestOAuthPending::default())),
    )
    .await
    .expect("OpenAI bundle");

    let quota = bundle
        .admin_provider()
        .quota(ProviderQuotaRequest {
            account_id: account.id().clone(),
            refresh: false,
            rolling_usage: None,
        })
        .await
        .expect("cached quota");
    let monthly = quota
        .windows
        .iter()
        .filter(|window| window.group == "monthly")
        .collect::<Vec<_>>();

    assert_eq!(monthly.len(), 1);
    assert!(monthly.iter().any(|window| {
        window.label == "月额度"
            && window.limit_id.as_deref() == Some("codex")
            && window.used_percent == Some(91.0)
    }));
    assert!(
        !quota
            .windows
            .iter()
            .any(|window| window.key.starts_with("additional-0-codex")),
        "the additional codex alias should not become a second display bucket"
    );
    let secondary = quota
        .windows
        .iter()
        .find(|window| {
            window.limit_id.as_deref() == Some("codex")
                && window.role == Some(ProviderQuotaWindowRole::Secondary)
        })
        .expect("core secondary quota");
    assert_eq!(secondary.label, "次级额度");
    assert_eq!(secondary.used_percent, Some(88.0));
    let review = quota
        .windows
        .iter()
        .find(|window| window.limit_id.as_deref() == Some("code_review"))
        .expect("code review quota");
    assert_eq!(review.label, "周额度");
    assert_eq!(review.limit_name.as_deref(), Some("code_review"));
    assert_eq!(review.role, Some(ProviderQuotaWindowRole::Primary));
    let spark = quota
        .windows
        .iter()
        .find(|window| window.limit_id.as_deref() == Some("codex_bengalfox"))
        .expect("Spark quota");
    assert_eq!(
        (
            monthly[0].local_usage_attribution,
            review.local_usage_attribution,
            spark.local_usage_attribution,
        ),
        (
            QuotaLocalUsageAttribution::AccountWide,
            QuotaLocalUsageAttribution::Unavailable,
            QuotaLocalUsageAttribution::Unavailable,
        ),
    );
}

#[tokio::test]
async fn openai_admin_keeps_confirmed_exhaustion_separate_from_raw_usage_display() {
    let store = Arc::new(MemoryAccountStore::default());
    let account_id = "acct_admin_confirmed_exhaustion";
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: account_id.to_owned(),
            name: "admin confirmed exhaustion".to_owned(),
            secret: secret("admin-confirmed-exhaustion-access"),
            verified_account: profile("chatgpt-admin-confirmed-exhaustion"),
            next_refresh_at: Some(Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    let account = store.account(account_id).expect("stored account");
    let reset_at = 1_900_000_000_u64;
    let observed_at = SystemTime::now();
    store
        .compare_and_swap_quota(QuotaObservation {
            plan_type: None,
            account_id: account.id().clone(),
            expected_revision: account.revision(),
            quota: OpaqueProviderData::new(
                json!({
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {"used_percent": 86, "reset_at": reset_at}
                    }
                })
                .as_object()
                .expect("quota object")
                .clone(),
            ),
            observed_at,
            state: QuotaState::allowed(observed_at),
        })
        .await
        .expect("persist raw quota");
    store
        .apply_quota_access(QuotaAccessChange {
            account_id: account.id().clone(),
            expected_revision: account.revision(),
            state: QuotaState::exhausted(QuotaEvidence::UsageLimitReached, SystemTime::now(), None),
        })
        .await
        .expect("mark account exhausted");
    let config = valid_config();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(Arc::clone(&store), Arc::new(TestOAuthPending::default())),
    )
    .await
    .expect("OpenAI bundle");

    let exhausted = bundle
        .admin_provider()
        .quota(ProviderQuotaRequest {
            account_id: account.id().clone(),
            refresh: false,
            rolling_usage: None,
        })
        .await
        .expect("project exhausted quota");

    assert_eq!(exhausted.windows.len(), 1);
    assert_eq!(exhausted.windows[0].used_percent, Some(86.0));
    let raw = store
        .get_quotas(std::slice::from_ref(account.id()))
        .await
        .expect("read raw quota")
        .pop()
        .expect("raw quota");
    assert_eq!(
        raw.quota.expose_to_provider()["rate_limit"]["primary_window"]["used_percent"],
        86
    );

    store
        .apply_quota_access(QuotaAccessChange {
            account_id: account.id().clone(),
            expected_revision: account.revision(),
            state: QuotaState::allowed(SystemTime::now()),
        })
        .await
        .expect("recover account");
    let recovered = bundle
        .admin_provider()
        .quota(ProviderQuotaRequest {
            account_id: account.id().clone(),
            refresh: false,
            rolling_usage: None,
        })
        .await
        .expect("project recovered quota");

    assert_eq!(recovered.windows[0].used_percent, Some(86.0));
}

#[tokio::test]
async fn openai_admin_preserves_expired_window_usage_and_exhaustion_attribution() {
    for exhausted in [false, true] {
        let store = Arc::new(MemoryAccountStore::default());
        let account_id = "acct_admin_expired_window";
        store
            .seed_oauth_credential(ImportCodexOAuthCredential {
                account_id: account_id.to_owned(),
                name: "admin expired window".to_owned(),
                secret: secret("admin-expired-window-access"),
                verified_account: profile("chatgpt-admin-expired-window"),
                next_refresh_at: Some(Utc::now() + chrono::Duration::minutes(30)),
                enabled: true,
            })
            .await;
        let account = store.account(account_id).expect("stored account");
        let past_reset_at = Utc::now().timestamp() - 60;
        let observed_at = SystemTime::now() - Duration::from_secs(300);
        let weekly_used = if exhausted { 100.0 } else { 74.0 };
        let state = if exhausted {
            QuotaState::exhausted(
                QuotaEvidence::ProviderDenied,
                observed_at,
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(past_reset_at as u64)),
            )
        } else {
            QuotaState::allowed(observed_at)
        };
        store
            .compare_and_swap_quota(QuotaObservation {
                plan_type: None,
                account_id: account.id().clone(),
                expected_revision: account.revision(),
                quota: OpaqueProviderData::new(
                    json!({
                        "rate_limit": {
                            "allowed": !exhausted,
                            "limit_reached": exhausted,
                            "primary_window": {
                                "used_percent": 15,
                                "reset_at": past_reset_at + 18_000,
                                "limit_window_seconds": 18_000,
                            },
                            "secondary_window": {
                                "used_percent": weekly_used,
                                "reset_at": past_reset_at,
                                "limit_window_seconds": 604_800,
                            }
                        }
                    })
                    .as_object()
                    .expect("quota object")
                    .clone(),
                ),
                observed_at,
                state,
            })
            .await
            .expect("persist raw quota");

        let config = valid_config();
        let bundle = provider_openai::initialize(
            config.config.clone(),
            provider_ports_with(Arc::clone(&store), Arc::new(TestOAuthPending::default())),
        )
        .await
        .expect("OpenAI bundle");
        let mut projected = bundle
            .admin_provider()
            .quota(ProviderQuotaRequest {
                account_id: account.id().clone(),
                refresh: false,
                rolling_usage: None,
            })
            .await
            .expect("project quota");
        assert_eq!(projected.limit_reached, exhausted);
        // 账号接口还会归一化耗尽展示；过期周窗口不能把触顶错误转移到短期窗口。
        projected.apply_limit_reached_display();
        let primary = projected
            .windows
            .iter()
            .find(|w| w.window_seconds == Some(18_000))
            .expect("primary");
        let weekly = projected
            .windows
            .iter()
            .find(|w| w.window_seconds == Some(604_800))
            .expect("weekly");
        assert_eq!(
            (primary.used_percent, primary.limit_reached),
            (Some(15.0), false)
        );
        assert_eq!(
            (weekly.used_percent, weekly.limit_reached),
            (Some(weekly_used), exhausted)
        );
        assert_eq!(
            weekly.reset_at.map(|reset| reset.timestamp()),
            Some(past_reset_at)
        );
        let raw = store
            .get_quotas(std::slice::from_ref(account.id()))
            .await
            .expect("raw quota")
            .pop()
            .expect("observation");
        assert_eq!(raw.observed_at, observed_at);
        assert_eq!(
            raw.quota.expose_to_provider()["rate_limit"]["secondary_window"]["used_percent"],
            weekly_used
        );
    }
}

#[tokio::test]
async fn openai_admin_provider_rejects_unprepared_mutations_before_store_commit() {
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_admin_invalid".to_owned(),
            name: "admin invalid".to_owned(),
            secret: secret("admin-invalid-access"),
            verified_account: profile("chatgpt-admin-invalid"),
            next_refresh_at: Some(chrono::Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    let account = store.account("acct_admin_invalid").expect("stored account");
    let record = account_record(&account);
    let config = valid_config();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(store, Arc::new(TestOAuthPending::default())),
    )
    .await
    .expect("OpenAI bundle");
    let admin = bundle.admin_provider();
    let import_error = admin
        .prepare_import(PrepareCredentialImport {
            default_outbound_proxy: None,
            document: ProviderDocument::new(OpaqueProviderData::new(Map::new())),
        })
        .await
        .expect_err("invalid import");
    assert_eq!(import_error.kind(), ProviderAdminErrorKind::Invalid);
    let mut stale_record = record.clone();
    stale_record.name = "stale name".to_owned();
    stale_record.email = None;
    stale_record.plan_type = None;
    stale_record.credential_revision = Revision::new(99).expect("stale revision");
    stale_record.has_refresh_token = false;
    stale_record.access_token_expires_at = None;
    stale_record.next_refresh_at = None;
    stale_record.enabled = false;
    stale_record.credential_state = CredentialState::Banned;
    let rotation_error = admin
        .prepare_rotation(PrepareCredentialRotation {
            account: stale_record,
            provider_material: ProviderDocument::new(OpaqueProviderData::new(Map::new())),
        })
        .await
        .expect_err("invalid rotation");
    assert_eq!(rotation_error.kind(), ProviderAdminErrorKind::Invalid);
    let mut missing = record;
    missing.id = "acct_admin_missing".to_owned();
    let refresh_error = admin
        .prepare_refresh(PrepareCredentialRefresh { account: missing })
        .await
        .expect_err("missing refresh target");
    assert_eq!(refresh_error.kind(), ProviderAdminErrorKind::NotFound);
}

#[tokio::test]
async fn initialized_provider_reports_a_safe_pat_format_error_before_network_access() {
    let config = valid_config();
    let bundle = provider_openai::initialize(config.config.clone(), provider_ports())
        .await
        .expect("OpenAI bundle");
    let error = bundle
        .admin_provider()
        .prepare_import(PrepareCredentialImport {
            default_outbound_proxy: None,
            document: ProviderDocument::new(OpaqueProviderData::new(Map::from_iter([(
                "accessToken".to_owned(),
                json!("at-sensitive-token with whitespace"),
            )]))),
        })
        .await
        .expect_err("PAT format must be checked by the initialized provider");
    assert_eq!(error.kind(), ProviderAdminErrorKind::Invalid);
    assert_eq!(
        error.public_message(),
        Some("Codex PAT 格式无效：应为 at- 开头的完整令牌，不能包含空白或控制字符")
    );
    assert!(error.message().is_none());
    assert!(!format!("{error:?}").contains("sensitive-token"));
}

#[tokio::test]
async fn openai_rotation_preserves_the_new_access_token_jwt_expiration() {
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_admin_rotation_expiration".to_owned(),
            name: "admin rotation expiration".to_owned(),
            secret: secret("admin-rotation-access"),
            verified_account: profile("chatgpt-admin-rotation-expiration"),
            next_refresh_at: None,
            enabled: true,
        })
        .await;
    let account = store
        .account("acct_admin_rotation_expiration")
        .expect("stored account");
    let record = account_record(&account);
    let config = valid_config();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(store, Arc::new(TestOAuthPending::default())),
    )
    .await
    .expect("OpenAI bundle");

    let expires_at = Utc
        .timestamp_opt(2_000_000_000, 0)
        .single()
        .expect("valid test timestamp");
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({"exp": expires_at.timestamp()}))
            .expect("test JWT payload"),
    );
    let mut material = Map::new();
    material.insert(
        "access_token".to_owned(),
        Value::String(format!("unverified-header.{payload}.unverified-signature")),
    );
    material.insert(
        "refresh_token".to_owned(),
        Value::String("admin-rotation-refresh".to_owned()),
    );

    let prepared = bundle
        .admin_provider()
        .prepare_rotation(PrepareCredentialRotation {
            account: record,
            provider_material: ProviderDocument::new(OpaqueProviderData::new(material)),
        })
        .await
        .expect("JWT rotation should be prepared");

    assert_eq!(prepared.facts().access_token_expires_at, Some(expires_at));
}

async fn turn_state_fixture(
    base: &MockServer,
    account_ids: &[&str],
) -> (
    Arc<provider_openai::ProviderBundle>,
    Arc<MemoryAccountStore>,
) {
    turn_state_fixture_with_leases(base, account_ids, Arc::new(TestLeaseCoordinator::default()))
        .await
}

async fn turn_state_fixture_with_leases(
    base: &MockServer,
    account_ids: &[&str],
    leases: Arc<TestLeaseCoordinator>,
) -> (
    Arc<provider_openai::ProviderBundle>,
    Arc<MemoryAccountStore>,
) {
    let store = Arc::new(MemoryAccountStore::default());
    for account_id in account_ids {
        store
            .seed_oauth_credential(ImportCodexOAuthCredential {
                account_id: (*account_id).to_owned(),
                name: format!("turn state {account_id}"),
                secret: secret(&format!("synthetic-{account_id}-access")),
                verified_account: profile(&format!("chatgpt-{account_id}")),
                next_refresh_at: Some(Utc::now() + chrono::Duration::minutes(30)),
                enabled: true,
            })
            .await;
    }
    let mut config = valid_config();
    config.config.api.base_url = base.uri();
    let bundle = provider_openai::initialize(
        config.config,
        provider_ports_with_catalog_and_leases(
            Arc::clone(&store),
            Arc::new(TestOAuthPending::default()),
            Arc::new(TestCatalogCache::default()),
            leases,
        ),
    )
    .await
    .expect("turn state fixture");
    (Arc::new(bundle), store)
}

fn upstream_model(model: &str) -> UpstreamModelId {
    UpstreamModelId::new(model).expect("upstream model")
}

fn probe_target(id: &str, server: &MockServer) -> TurnStateProbeTarget {
    TurnStateProbeTarget {
        id: id.to_owned(),
        label: format!("测试出口 {id}"),
        proxy: OutboundProxy::parse(&server.uri()).expect("localhost proxy"),
    }
}

fn probe_targets(server: &MockServer, ids: &[&str]) -> Vec<TurnStateProbeTarget> {
    ids.iter().map(|id| probe_target(id, server)).collect()
}

fn probe_response(status: u16, state: Option<char>) -> ResponseTemplate {
    let template = ResponseTemplate::new(status).set_body_json(json!({"status": status}));
    state.map_or(template.clone(), |character| {
        template.insert_header("x-codex-turn-state", character.to_string().repeat(292))
    })
}

async fn mount_turn_state_sequence(
    server: &MockServer,
    seen: Arc<Notify>,
    templates: Vec<ResponseTemplate>,
) -> Arc<AtomicUsize> {
    let expected = templates.len();
    let templates = Arc::new(templates);
    let next = Arc::new(AtomicUsize::new(0));
    let request_count = Arc::clone(&next);
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(move |_request: &wiremock::Request| {
            let index = next.fetch_add(1, Ordering::SeqCst);
            seen.notify_one();
            templates
                .get(index)
                .cloned()
                .expect("unexpected turn state request")
        })
        .expect(expected as u64)
        .mount(server)
        .await;
    request_count
}

async fn wait_for_request(seen: &Notify, request_count: &AtomicUsize, expected: usize) {
    loop {
        // 先登记等待，再检查计数，避免请求恰好落在检查与 await 之间。
        let notified = seen.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if request_count.load(Ordering::SeqCst) >= expected {
            return;
        }
        notified.await;
    }
}

struct PausedTimeGuard {
    keep_ready: tokio::task::JoinHandle<()>,
}

impl PausedTimeGuard {
    fn new() -> Self {
        tokio::time::pause();
        let keep_ready = tokio::spawn(async {
            loop {
                // 始终保留一个就绪任务，虚拟时钟只能由测试显式 advance。
                tokio::task::yield_now().await;
            }
        });
        Self { keep_ready }
    }
}

impl Drop for PausedTimeGuard {
    fn drop(&mut self) {
        self.keep_ready.abort();
    }
}

fn turn_state_backoff(account_id: &str, model: Option<&str>, failure_count: u64) -> Duration {
    let exponent = failure_count.saturating_sub(1).min(4) as u32;
    let base = (120_u64.saturating_mul(1_u64 << exponent)).min(1_800);
    let mut seed = 0_u64;
    for byte in account_id.as_bytes() {
        seed = seed.wrapping_mul(31).wrapping_add(u64::from(*byte));
    }
    if let Some(model) = model {
        seed = seed.wrapping_mul(31);
        for byte in model.as_bytes() {
            seed = seed.wrapping_mul(31).wrapping_add(u64::from(*byte));
        }
    }
    let jitter = seed.wrapping_add(failure_count.wrapping_mul(17)) % 31;
    Duration::from_secs(base.saturating_add(jitter).min(1_800))
}

async fn drain_turn_state_business(
    bundle: &provider_openai::ProviderBundle,
    account_id: &str,
    model: &str,
    request_id: &str,
    expect_success: bool,
) {
    let payload = ProtocolPayload::json_object(
        "openai",
        Map::from_iter([
            ("model".to_owned(), json!(model)),
            ("input".to_owned(), json!("turn state business request")),
        ]),
    )
    .expect("business payload")
    .with_context(Map::from_iter([("use_websocket".to_owned(), json!(false))]));
    let operation = Operation::Generate(GenerateRequest::from_protocol_payload(payload));
    let mut stream = bundle
        .core_provider()
        .execute(
            initialized_provider_request_for_model(operation, account_id, model),
            initialized_attempt_context(request_id, account_id),
        )
        .await
        .expect("prepare turn state business request");
    while let Some(event) = stream.next().await {
        if expect_success {
            event.expect("turn state business response");
        }
    }
}

async fn reset_credit_admin(
    server: &MockServer,
) -> (
    provider_openai::ProviderBundle,
    ProviderAccountId,
    TestOpenAiConfig,
) {
    let account_id = ProviderAccountId::new("acct_reset_credit").expect("account ID");
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: account_id.to_string(),
            name: "reset credit".to_owned(),
            secret: secret("reset-credit-access"),
            verified_account: profile("chatgpt-reset-credit"),
            next_refresh_at: Some(Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    let mut config = valid_config();
    config.config.api.base_url = server.uri();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(store, Arc::new(TestOAuthPending::default())),
    )
    .await
    .expect("OpenAI reset-credit bundle");
    (bundle, account_id, config)
}

fn reset_credit_command(account_id: ProviderAccountId) -> ConsumeProviderResetCredit {
    ConsumeProviderResetCredit {
        account_id,
        credit_id: Some("credit_1".to_owned()),
        redeem_request_id: Uuid::parse_str("8fbf302d-11df-4bd5-82e4-08e4b3df7874")
            .expect("UUID v4"),
    }
}

fn initialized_provider_request(operation: Operation, account_id: &str) -> ProviderRequest {
    initialized_provider_request_for_model(operation, account_id, "gpt-5.4")
}

fn initialized_provider_request_for_model(
    operation: Operation,
    account_id: &str,
    model: &str,
) -> ProviderRequest {
    initialized_provider_request_for_scope(operation, initialized_account_scope(account_id), model)
}

fn initialized_provider_request_for_scope(
    operation: Operation,
    account_scope: Arc<FrozenAccountScope>,
    model: &str,
) -> ProviderRequest {
    let provider = ProviderKind::new("openai").expect("provider");
    let upstream_model = UpstreamModelId::new(model).expect("upstream model");
    let public_model = PublicModelId::new(upstream_model.as_str()).expect("public model");
    let snapshot = RuntimeSnapshot::new(
        ConfigRevision::new(1).expect("revision"),
        account_policy(),
        vec![provider.clone()],
        vec![ProviderModel::new(
            provider,
            upstream_model,
            ModelCapabilities::new(BTreeSet::from([operation.kind()]), Some(32_000))
                .with_upstream_feature_validation(),
        )],
        Vec::new(),
    )
    .expect("runtime snapshot");
    let plan = snapshot
        .plan(
            &public_model,
            &operation,
            account_scope,
            &RoutingContext::default(),
        )
        .expect("routing plan");

    ProviderRequest::new(operation, plan.candidates()[0].clone())
}

fn initialized_attempt_context(request_id: &str, account_id: &str) -> AttemptContext {
    initialized_attempt_context_for_scope(request_id, initialized_account_scope(account_id))
}

fn initialized_attempt_context_for_scope(
    request_id: &str,
    account_scope: Arc<FrozenAccountScope>,
) -> AttemptContext {
    AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new(request_id).expect("request id"),
            ClientApiKeyId::new("key_openai_initialized").expect("client key id"),
        ),
        NonZeroU32::new(1).expect("attempt"),
        SystemTime::now() + Duration::from_secs(30),
        account_policy(),
        AccountAttemptContext::new(BTreeSet::new(), None, None).with_account_scope(account_scope),
        None,
        CancellationToken::new(),
    )
}

fn initialized_account_scope(account_id: &str) -> Arc<FrozenAccountScope> {
    let provider = ProviderKind::new("openai").expect("provider");
    Arc::new(FrozenAccountScope::new(
        Arc::new(RuntimeAccountDirectory::new(BTreeMap::from([(
            ProviderAccountId::new(account_id).expect("account id"),
            RuntimeAccount::new(provider, BTreeSet::new()),
        )]))),
        ClientRoutingScope::all_accounts(),
    ))
}

fn provider_ports() -> ProviderStorePorts {
    provider_ports_with(
        Arc::new(MemoryAccountStore::default()),
        Arc::new(TestOAuthPending::default()),
    )
}

fn provider_ports_with(
    accounts: Arc<MemoryAccountStore>,
    pending: Arc<TestOAuthPending>,
) -> ProviderStorePorts {
    provider_ports_with_catalog(accounts, pending, Arc::new(TestCatalogCache::default()))
}

fn provider_ports_with_catalog(
    accounts: Arc<MemoryAccountStore>,
    pending: Arc<TestOAuthPending>,
    catalog_cache: Arc<TestCatalogCache>,
) -> ProviderStorePorts {
    provider_ports_with_catalog_and_leases(
        accounts,
        pending,
        catalog_cache,
        Arc::new(TestLeaseCoordinator::default()),
    )
}

fn provider_ports_with_catalog_and_leases(
    accounts: Arc<MemoryAccountStore>,
    pending: Arc<TestOAuthPending>,
    catalog_cache: Arc<TestCatalogCache>,
    leases: Arc<TestLeaseCoordinator>,
) -> ProviderStorePorts {
    ProviderStorePorts::new(
        accounts,
        leases,
        Arc::new(MemorySessionAffinity::default()),
        Arc::new(MemorySessionExclusions::default()),
        catalog_cache,
        Arc::new(TestArtifactProfiles),
        Arc::new(TestCredentialState),
        Arc::new(TestCooldown),
        Arc::new(TestRuntimePolicy),
        pending,
    )
}

fn account_record(account: &ProviderAccount) -> AccountRecord {
    let now = Utc::now();
    AccountRecord {
        notes: None,
        model_access: Default::default(),
        outbound_proxy: None,
        id: account.id().to_string(),
        provider_kind: account.provider().clone(),
        groups: Vec::new(),
        name: account.name().to_owned(),
        email: account.email().map(str::to_owned),
        upstream_user_id: account.upstream_user_id().map(str::to_owned),
        upstream_account_id: account.upstream_account_id().map(str::to_owned),
        plan_type: account.plan_type().map(str::to_owned),
        authentication_kind: account.authentication_kind().to_owned(),
        credential_revision: Revision::new(account.revision().get()).expect("revision"),
        has_refresh_token: account.has_refresh_token(),
        access_token_expires_at: account.access_token_expires_at().map(DateTime::<Utc>::from),
        next_refresh_at: account.next_refresh_at().map(DateTime::<Utc>::from),
        enabled: account.enabled(),
        concurrency_limit: account.concurrency_limit(),
        weight: account.weight(),
        credential_state: account.credential_state(),
        credential_observed_at: now,
        quota: account.quota(),
        last_error_reason: account.last_error_reason(),
        last_error_message: None,
        created_at: now,
        updated_at: now,
    }
}

struct TestOpenAiConfig {
    config: OpenAiConfig,
    _runtime: TempDir,
}

fn valid_config() -> TestOpenAiConfig {
    let mut config = OpenAiConfig::default();
    let runtime = tempfile::tempdir().expect("test runtime directory");
    config
        .resolve_and_validate(&runtime.path().join("deploy"))
        .expect("valid OpenAI test configuration");
    TestOpenAiConfig {
        config,
        _runtime: runtime,
    }
}

struct TestArtifactProfiles;

impl ProviderArtifactProfileCachePort for TestArtifactProfiles {
    fn replace_if_newer(
        &self,
        _profile: ProviderArtifactProfile,
        _ttl: Duration,
    ) -> BoxFuture<'_, Result<bool, ProviderStoreError>> {
        Box::pin(async { Ok(true) })
    }

    fn read<'a>(
        &'a self,
        _provider_kind: &'a ProviderKind,
        _artifact_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProviderArtifactProfile>, ProviderStoreError>> {
        Box::pin(async { Ok(None) })
    }
}

#[derive(Default)]
struct TestCatalogCache {
    values: Mutex<BTreeMap<String, OpaqueProviderData>>,
}

impl ProviderCatalogCachePort for TestCatalogCache {
    fn replace<'a>(
        &'a self,
        key: &'a ProviderCatalogCacheKey,
        catalog: &'a OpaqueProviderData,
        _ttl: Duration,
    ) -> BoxFuture<'a, Result<(), ProviderStoreError>> {
        Box::pin(async move {
            self.values
                .lock()
                .expect("catalog cache")
                .insert(key.scope().as_str().to_owned(), catalog.clone());
            Ok(())
        })
    }

    fn read<'a>(
        &'a self,
        key: &'a ProviderCatalogCacheKey,
    ) -> BoxFuture<'a, Result<Option<OpaqueProviderData>, ProviderStoreError>> {
        Box::pin(async move {
            Ok(self
                .values
                .lock()
                .expect("catalog cache")
                .get(key.scope().as_str())
                .cloned())
        })
    }
}

impl TestCatalogCache {
    fn seed(&self, scope: &str, models: impl IntoIterator<Item = &'static str>) {
        let mut document = Map::new();
        document.insert("version".to_owned(), Value::from(1));
        document.insert("scope".to_owned(), Value::String(scope.to_owned()));
        document.insert(
            "observedAt".to_owned(),
            Value::String(Utc::now().to_rfc3339()),
        );
        document.insert(
            "models".to_owned(),
            Value::Array(
                models
                    .into_iter()
                    .map(|model| Value::String(model.to_owned()))
                    .collect(),
            ),
        );
        self.values
            .lock()
            .expect("catalog cache")
            .insert(scope.to_owned(), OpaqueProviderData::new(document));
    }
}

struct TestCredentialState;

impl ProviderCredentialStatePort for TestCredentialState {
    fn replace(
        &self,
        _state: ProviderCredentialState,
    ) -> BoxFuture<'_, Result<(), ProviderStoreError>> {
        Box::pin(async { Ok(()) })
    }

    fn read<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
    ) -> BoxFuture<'a, Result<Option<ProviderCredentialState>, ProviderStoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn clear<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
    ) -> BoxFuture<'a, Result<bool, ProviderStoreError>> {
        Box::pin(async { Ok(false) })
    }

    fn record_refresh_backoff<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
        _window: Duration,
    ) -> BoxFuture<'a, Result<u32, ProviderStoreError>> {
        Box::pin(async { Ok(1) })
    }

    fn clear_refresh_backoff<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
    ) -> BoxFuture<'a, Result<(), ProviderStoreError>> {
        Box::pin(async { Ok(()) })
    }
}

struct TestCooldown;

impl ProviderCooldownPort for TestCooldown {
    fn put_if_later(
        &self,
        _cooldown: ProviderCooldown,
    ) -> BoxFuture<'_, Result<bool, ProviderStoreError>> {
        Box::pin(async { Ok(false) })
    }

    fn read<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
    ) -> BoxFuture<'a, Result<Option<ProviderCooldown>, ProviderStoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn clear<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
        _through_revision: CredentialRevision,
    ) -> BoxFuture<'a, Result<bool, ProviderStoreError>> {
        Box::pin(async { Ok(false) })
    }

    fn put_scoped_if_later(
        &self,
        _cooldown: ProviderScopedCooldown,
    ) -> BoxFuture<'_, Result<bool, ProviderStoreError>> {
        Box::pin(async { Ok(false) })
    }

    fn read_scoped<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
        _scope: &'a ProviderCooldownScope,
    ) -> BoxFuture<'a, Result<Option<ProviderScopedCooldown>, ProviderStoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn clear_scoped<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
        _scope: &'a ProviderCooldownScope,
        _through_revision: CredentialRevision,
    ) -> BoxFuture<'a, Result<bool, ProviderStoreError>> {
        Box::pin(async { Ok(false) })
    }

    fn clear_all<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
    ) -> BoxFuture<'a, Result<bool, ProviderStoreError>> {
        Box::pin(async { Ok(false) })
    }

    fn record_capacity_failure<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
        _window: Duration,
        _in_flight: u32,
    ) -> BoxFuture<'a, Result<u32, ProviderStoreError>> {
        Box::pin(async { Ok(0) })
    }

    fn clear_after_success<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
        _through_revision: gateway_core::account::CredentialRevision,
    ) -> BoxFuture<'a, Result<(), ProviderStoreError>> {
        Box::pin(async { Ok(()) })
    }

    fn capacity_peak_in_flight<'a>(
        &'a self,
        _account_id: &'a ProviderAccountId,
    ) -> BoxFuture<'a, Result<Option<u32>, ProviderStoreError>> {
        Box::pin(async { Ok(None) })
    }
}

struct TestRuntimePolicy;

impl ProviderRuntimePolicyPort for TestRuntimePolicy {
    fn load_refresh_policy(
        &self,
    ) -> BoxFuture<'_, Result<ProviderRefreshPolicy, ProviderStoreError>> {
        Box::pin(async {
            ProviderRefreshPolicy::try_new(
                Duration::from_secs(300),
                NonZeroU32::new(4).expect("nonzero concurrency"),
            )
        })
    }
}

#[derive(Default)]
struct TestOAuthPending {
    values: Mutex<BTreeMap<PendingKey, PendingValue>>,
}

type PendingKey = (String, String);
type PendingValue = (String, OpaqueProviderData, SystemTime, Option<String>);

impl OAuthPendingFlowPort for TestOAuthPending {
    fn put_if_absent(
        &self,
        flow: NewOAuthPendingFlow,
    ) -> BoxFuture<'_, Result<OAuthPendingPutOutcome, ProviderStoreError>> {
        Box::pin(async move {
            let key = (
                flow.provider_kind().as_str().to_owned(),
                flow.flow().expose_to_store().to_owned(),
            );
            let mut values = self.values.lock().expect("OAuth pending");
            if values.contains_key(&key) {
                return Ok(OAuthPendingPutOutcome::AlreadyExists);
            }
            values.insert(
                key,
                (
                    flow.owner().expose_to_store().to_owned(),
                    flow.payload().clone(),
                    SystemTime::now() + flow.ttl(),
                    None,
                ),
            );
            Ok(OAuthPendingPutOutcome::Stored)
        })
    }

    fn claim_if_owner<'a>(
        &'a self,
        provider_kind: &'a ProviderKind,
        flow: &'a gateway_core::provider_ports::OAuthPendingBinding,
        owner: &'a gateway_core::provider_ports::OAuthPendingBinding,
        claim: &'a gateway_core::provider_ports::OAuthPendingBinding,
        _claim_ttl: Duration,
    ) -> BoxFuture<'a, Result<OAuthPendingClaimOutcome, ProviderStoreError>> {
        Box::pin(async move {
            let key = (
                provider_kind.as_str().to_owned(),
                flow.expose_to_store().to_owned(),
            );
            let mut values = self.values.lock().expect("OAuth pending");
            let Some((stored_owner, payload, expires_at, stored_claim)) = values.get_mut(&key)
            else {
                return Ok(OAuthPendingClaimOutcome::NotFound);
            };
            if *expires_at <= SystemTime::now() {
                values.remove(&key);
                return Ok(OAuthPendingClaimOutcome::NotFound);
            }
            if stored_owner != owner.expose_to_store() {
                return Ok(OAuthPendingClaimOutcome::OwnerMismatch);
            }
            if stored_claim.is_some() {
                return Ok(OAuthPendingClaimOutcome::InProgress);
            }
            *stored_claim = Some(claim.expose_to_store().to_owned());
            Ok(OAuthPendingClaimOutcome::Claimed(payload.clone()))
        })
    }

    fn release_claim<'a>(
        &'a self,
        provider_kind: &'a ProviderKind,
        flow: &'a gateway_core::provider_ports::OAuthPendingBinding,
        owner: &'a gateway_core::provider_ports::OAuthPendingBinding,
        claim: &'a gateway_core::provider_ports::OAuthPendingBinding,
    ) -> BoxFuture<'a, Result<OAuthPendingReleaseOutcome, ProviderStoreError>> {
        Box::pin(async move {
            let key = (
                provider_kind.as_str().to_owned(),
                flow.expose_to_store().to_owned(),
            );
            let mut values = self.values.lock().expect("OAuth pending");
            let Some((stored_owner, _, _, stored_claim)) = values.get_mut(&key) else {
                return Ok(OAuthPendingReleaseOutcome::NotFound);
            };
            if stored_owner != owner.expose_to_store() {
                return Ok(OAuthPendingReleaseOutcome::OwnerMismatch);
            }
            if stored_claim.as_deref() != Some(claim.expose_to_store()) {
                return Ok(OAuthPendingReleaseOutcome::ClaimMismatch);
            }
            *stored_claim = None;
            Ok(OAuthPendingReleaseOutcome::Released)
        })
    }

    fn consume_claim<'a>(
        &'a self,
        provider_kind: &'a ProviderKind,
        flow: &'a gateway_core::provider_ports::OAuthPendingBinding,
        owner: &'a gateway_core::provider_ports::OAuthPendingBinding,
        claim: &'a gateway_core::provider_ports::OAuthPendingBinding,
    ) -> BoxFuture<'a, Result<OAuthPendingConsumeOutcome, ProviderStoreError>> {
        Box::pin(async move {
            let key = (
                provider_kind.as_str().to_owned(),
                flow.expose_to_store().to_owned(),
            );
            let mut values = self.values.lock().expect("OAuth pending");
            let Some((stored_owner, _, _, stored_claim)) = values.get(&key) else {
                return Ok(OAuthPendingConsumeOutcome::NotFound);
            };
            if stored_owner != owner.expose_to_store() {
                return Ok(OAuthPendingConsumeOutcome::OwnerMismatch);
            }
            if stored_claim.as_deref() != Some(claim.expose_to_store()) {
                return Ok(OAuthPendingConsumeOutcome::ClaimMismatch);
            }
            values.remove(&key);
            Ok(OAuthPendingConsumeOutcome::Consumed)
        })
    }
}

mod errors {
    use gateway_admin::ports::provider::ProviderAdminErrorKind as Kind;
    use gateway_core::provider_ports::{
        ProviderLeaseAcquisition, ProviderLeasePort, ProviderLeaseRequest, ProviderSchedulingState,
    };

    use super::*;

    #[tokio::test]
    async fn manual_refresh_preserves_banned_evidence_without_promoting_401_to_terminal() {
        for (status, kind, message) in [
            (400, Kind::Invalid, "OpenAI 账号已被停用，请检查账号状态"),
            (
                401,
                Kind::BadGateway,
                "OpenAI 拒绝了令牌刷新，请检查账号授权状态",
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/oauth/token"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "error": {
                        "message": "account has been deactivated: raw-secret-marker"
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;
            let (bundle, store, _config) = refresh_fixture(&server, false, true).await;
            let before = store.account("acct_refresh_error").unwrap();
            let error = bundle
                .admin_provider()
                .prepare_refresh(PrepareCredentialRefresh {
                    account: account_record(&before),
                })
                .await
                .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.public_message(), Some(message));
            assert!(!format!("{error:?} {error}").contains("raw-secret-marker"));
            assert_eq!(store.account("acct_refresh_error").unwrap(), before);
        }
    }

    #[tokio::test]
    async fn manual_refresh_reports_known_upstream_failures_without_changing_account_state() {
        for (status, code, expected_kind, expected_message) in [
            (
                401,
                "refresh_token_reused",
                Kind::BadGateway,
                "刷新令牌已被使用，请重新授权",
            ),
            (
                401,
                "refresh_token_expired",
                Kind::BadGateway,
                "刷新令牌已过期，请重新授权",
            ),
            (
                401,
                "refresh_token_invalidated",
                Kind::BadGateway,
                "刷新令牌已被撤销，请重新授权",
            ),
            (
                401,
                "token_expired",
                Kind::BadGateway,
                "刷新令牌不可用，请重新授权",
            ),
            (
                401,
                "unknown",
                Kind::BadGateway,
                "OpenAI 拒绝了令牌刷新，请检查账号授权状态",
            ),
            (
                400,
                "refresh_token_reused",
                Kind::Invalid,
                "刷新令牌已被使用，请重新授权",
            ),
            (
                400,
                "refresh_token_expired",
                Kind::Invalid,
                "刷新令牌已过期，请重新授权",
            ),
            (
                400,
                "refresh_token_invalidated",
                Kind::Invalid,
                "刷新令牌已被撤销，请重新授权",
            ),
            (
                400,
                "INVALID_GRANT",
                Kind::BadGateway,
                "刷新令牌无效或已失效，请重新授权",
            ),
            (
                429,
                "unknown",
                Kind::BadGateway,
                "OpenAI 令牌刷新请求被限流，请稍后重试",
            ),
            (
                503,
                "unknown",
                Kind::BadGateway,
                "OpenAI 令牌刷新服务异常，请稍后重试",
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/oauth/token"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "error": {"code": code, "message": "raw-secret-marker"}
                })))
                .expect(1)
                .mount(&server)
                .await;
            let (bundle, store, _config) = refresh_fixture(&server, false, true).await;
            let before = store.account("acct_refresh_error").unwrap();
            let error = bundle
                .admin_provider()
                .prepare_refresh(PrepareCredentialRefresh {
                    account: account_record(&before),
                })
                .await
                .expect_err("upstream rejection");
            assert_eq!(error.kind(), expected_kind, "HTTP {status} {code}");
            assert_eq!(error.public_message(), Some(expected_message));
            assert!(!format!("{error:?} {error}").contains("raw-secret-marker"));
            assert_eq!(store.account("acct_refresh_error").unwrap(), before);
        }
    }

    #[tokio::test]
    async fn manual_refresh_distinguishes_busy_missing_token_and_stale_account_before_exchange() {
        for (busy, has_token, stale, kind, message) in [
            (
                true,
                true,
                false,
                Kind::Conflict,
                "令牌刷新繁忙，请等待当前刷新完成后重试",
            ),
            (
                false,
                false,
                false,
                Kind::Invalid,
                "账号没有刷新令牌，请重新授权",
            ),
            (
                false,
                true,
                true,
                Kind::Conflict,
                "账号凭据已被更新，请刷新账号列表后重试",
            ),
        ] {
            let server = MockServer::start().await;
            let (bundle, store, _config) = refresh_fixture(&server, busy, has_token).await;
            let before = store.account("acct_refresh_error").unwrap();
            let mut account = account_record(&before);
            if stale {
                account.upstream_user_id = Some("previous-user".to_owned());
            }
            let error = bundle
                .admin_provider()
                .prepare_refresh(PrepareCredentialRefresh { account })
                .await
                .expect_err("local refresh failure");
            assert_eq!(error.kind(), kind);
            assert_eq!(error.public_message(), Some(message));
            assert!(server.received_requests().await.unwrap().is_empty());
            assert_eq!(store.account("acct_refresh_error").unwrap(), before);
        }
    }

    #[tokio::test]
    async fn manual_refresh_invalid_success_and_unclassified_transport_stay_conservative() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("invalid-success-secret-marker"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (bundle, store, mut config) = refresh_fixture(&server, false, true).await;
        let before = store.account("acct_refresh_error").unwrap();
        let command = || PrepareCredentialRefresh {
            account: account_record(&before),
        };
        let error = bundle
            .admin_provider()
            .prepare_refresh(command())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), Kind::Ambiguous);
        assert_eq!(
            error.public_message(),
            Some("令牌刷新结果未知，请先核对账号状态，不要立即重复刷新")
        );
        assert!(!format!("{error:?}").contains("invalid-success-secret-marker"));

        config.config.auth.oauth_token_endpoint = "http://127.0.0.1:0/oauth/token".to_owned();
        let bundle =
            provider_openai::initialize(config.config, refresh_ports(store.clone(), false))
                .await
                .unwrap();
        let error = bundle
            .admin_provider()
            .prepare_refresh(command())
            .await
            .unwrap_err();
        // 既有 transport 策略未认定此错误为安全重试，本次不能因展示更详细而放宽重试边界。
        assert_eq!(error.kind(), Kind::Ambiguous);
        assert_eq!(
            error.public_message(),
            Some("令牌刷新结果未知，请先核对账号状态，不要立即重复刷新")
        );
        assert_eq!(store.account("acct_refresh_error").unwrap(), before);
    }

    #[tokio::test]
    async fn manual_token_refresh_prepares_to_preserve_concurrent_profile_changes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new-access-token", "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (bundle, store, _config) = refresh_fixture(&server, false, true).await;
        let before = store.account("acct_refresh_error").unwrap();
        let prepared = bundle
            .admin_provider()
            .prepare_refresh(PrepareCredentialRefresh {
                account: account_record(&before),
            })
            .await
            .unwrap();
        assert!(prepared.facts().preserve_profile);
    }

    async fn refresh_fixture(
        server: &MockServer,
        busy: bool,
        has_token: bool,
    ) -> (
        provider_openai::ProviderBundle,
        Arc<MemoryAccountStore>,
        TestOpenAiConfig,
    ) {
        let store = Arc::new(MemoryAccountStore::default());
        let mut credential = secret("synthetic-refresh-access");
        if !has_token {
            credential.refresh_token = None;
        }
        store
            .seed_oauth_credential(ImportCodexOAuthCredential {
                account_id: "acct_refresh_error".to_owned(),
                name: "refresh error test".to_owned(),
                secret: credential,
                verified_account: profile("synthetic-refresh-user"),
                next_refresh_at: None,
                enabled: true,
            })
            .await;
        let mut config = valid_config();
        config.config.auth.oauth_token_endpoint = format!("{}/oauth/token", server.uri());
        let bundle =
            provider_openai::initialize(config.config.clone(), refresh_ports(store.clone(), busy))
                .await
                .unwrap();
        (bundle, store, config)
    }

    fn refresh_ports(store: Arc<MemoryAccountStore>, busy: bool) -> ProviderStorePorts {
        ProviderStorePorts::new(
            store,
            Arc::new(RefreshLeases { busy }),
            Arc::new(MemorySessionAffinity::default()),
            Arc::new(MemorySessionExclusions::default()),
            Arc::new(TestCatalogCache::default()),
            Arc::new(TestArtifactProfiles),
            Arc::new(TestCredentialState),
            Arc::new(TestCooldown),
            Arc::new(TestRuntimePolicy),
            Arc::new(TestOAuthPending::default()),
        )
    }

    struct RefreshLeases {
        busy: bool,
    }

    impl ProviderLeasePort for RefreshLeases {
        fn load_state<'a>(
            &'a self,
            _: &'a ClientApiKeyId,
            _: &'a ProviderKind,
            _: &'a [ProviderAccountId],
        ) -> BoxFuture<'a, Result<ProviderSchedulingState, ProviderStoreError>> {
            panic!("manual refresh does not use scheduling leases")
        }

        fn try_acquire(
            &self,
            request: ProviderLeaseRequest,
        ) -> BoxFuture<'_, Result<ProviderLeaseAcquisition, ProviderStoreError>> {
            assert!(matches!(
                request,
                ProviderLeaseRequest::Refresh(_) | ProviderLeaseRequest::RefreshCapacity(_)
            ));
            Box::pin(async move {
                Ok(if self.busy {
                    ProviderLeaseAcquisition::Busy { retry_after: None }
                } else {
                    ProviderLeaseAcquisition::Acquired(Box::new(()))
                })
            })
        }
    }
}

#[tokio::test]
async fn api_key_admin_exposes_only_configuration_and_preserves_key_when_rotating_address() {
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_api_key(
            "acct_api_admin",
            "https://first.example/v1".to_owned(),
            provider_openai::credential::ApiKeyTransport::Http,
        )
        .await;
    let account = store.account("acct_api_admin").unwrap();
    let config = valid_config();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(Arc::clone(&store), Arc::new(TestOAuthPending::default())),
    )
    .await
    .unwrap();
    let admin = bundle.admin_provider();
    let configuration = admin
        .account_configuration(account.id())
        .await
        .unwrap()
        .unwrap();
    let configuration = configuration.expose_to_provider().expose_to_provider();
    assert_eq!(configuration.len(), 2);
    assert_eq!(
        configuration.get("base_url"),
        Some(&json!("https://first.example/v1"))
    );
    assert!(!configuration.contains_key("api_key"));
    let prepared = admin
        .prepare_rotation(PrepareCredentialRotation {
            account: account_record(&account),
            provider_material: ProviderDocument::new(OpaqueProviderData::new(
                json!({"base_url":"https://second.example/root", "transport":"prefer_websocket"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )),
        })
        .await
        .unwrap();
    let material = prepared
        .facts()
        .provider_material
        .expose_to_provider()
        .expose_to_provider();
    assert_eq!(material.get("api_key"), Some(&json!("sk-api-test-only")));
    assert_eq!(
        material.get("base_url"),
        Some(&json!("https://second.example/root"))
    );
    assert!(!prepared.facts().has_refresh_token);
    assert_eq!(prepared.facts().account_id, *account.id());
    assert_eq!(
        admin
            .prepare_refresh(PrepareCredentialRefresh {
                account: account_record(&account)
            })
            .await
            .unwrap_err()
            .kind(),
        ProviderAdminErrorKind::Unsupported
    );
    assert_eq!(
        admin.subscription(account.id()).await.unwrap_err().kind(),
        ProviderAdminErrorKind::Unsupported
    );
    assert_eq!(
        admin.reset_credits(account.id()).await.unwrap_err().kind(),
        ProviderAdminErrorKind::Unsupported
    );
}
