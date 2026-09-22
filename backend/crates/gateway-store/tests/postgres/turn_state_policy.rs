use super::{TestDatabase, provider_accounts::audit};
use gateway_admin::{
    model::{
        MutationActor, MutationContext,
        proxies::NewProxy,
        turn_state::{
            ResponseHeaderCarryRule, ResponseHeaderCarryScope, ResponseHeaderCarrySource,
            ResponseHeaderMergeMode, ResponseHeaderMissingBehavior, ResponseHeaderTransform,
            ResponseHeaderValueSelection, TurnStateProbePolicy, TurnStateProxyMode,
        },
    },
    ports::{proxy::ProxyStore, store::AdminStoreErrorKind},
};
use gateway_core::account::OutboundProxy;
use gateway_store::postgres::{PgControlPlaneRepository, PgProxyRepository};

#[tokio::test]
async fn turn_state_policy_persists_switches_and_serializes_proxy_references() {
    let Some(database) = TestDatabase::create("turn_state_policy").await else {
        return;
    };
    let repository = PgControlPlaneRepository::new(database.pool.clone());
    let proxies = PgProxyRepository::new(database.pool.clone());
    let context = MutationContext {
        actor: MutationActor::System,
        request_id: "policy-test".to_owned(),
    };
    assert_eq!(
        repository.load_turn_state_probe_policy().await.unwrap(),
        TurnStateProbePolicy::default()
    );
    let proxy = proxies
        .create(
            NewProxy {
                name: "Policy proxy".to_owned(),
                proxy: OutboundProxy::parse("http://127.0.0.1:18081").unwrap(),
                location: None,
            },
            &context,
        )
        .await
        .unwrap()
        .record;
    let policy = TurnStateProbePolicy {
        manual_enabled: false,
        automatic_enabled: false,
        mode: TurnStateProxyMode::Fixed,
        proxy_ids: vec![proxy.id.clone()],
        candidate_limit: 1,
        response_header_carry: gateway_admin::model::turn_state::ResponseHeaderCarryPolicy {
            rules: vec![ResponseHeaderCarryRule {
                id: "cookie_bundle".to_owned(),
                name: "Cookie bundle".to_owned(),
                enabled: true,
                capture_enabled: true,
                injection_enabled: true,
                clear_on_disable: false,
                sources: vec![
                    ResponseHeaderCarrySource::BusinessResponse,
                    ResponseHeaderCarrySource::TurnStateProbe,
                ],
                source_header: "set-cookie".to_owned(),
                target_header: "cookie".to_owned(),
                transform: ResponseHeaderTransform::SetCookieToCookie,
                value_selection: ResponseHeaderValueSelection::All,
                merge_mode: ResponseHeaderMergeMode::Replace,
                scope: ResponseHeaderCarryScope::AccountModel,
                account_ids: Vec::new(),
                models: Vec::new(),
                ttl_seconds: 3_600,
                missing_behavior: ResponseHeaderMissingBehavior::Keep,
                capture_status_min: 200,
                capture_status_max: 299,
                invalidation_statuses: vec![401, 403],
                max_value_bytes: 8_192,
                max_values: 16,
            }],
        },
        ..Default::default()
    };
    repository
        .update_turn_state_probe_policy(
            policy.clone(),
            audit(
                "policy-save",
                "turn_state_probe_policy.update",
                "runtime_settings",
            ),
        )
        .await
        .unwrap();
    // 重新创建仓储验证数据库事实，不依赖同一实例内存。
    assert_eq!(
        PgControlPlaneRepository::new(database.pool.clone())
            .load_turn_state_probe_policy()
            .await
            .unwrap(),
        policy
    );
    // 通用运行设置更新不会覆盖单独保存的探测策略。
    use gateway_store::postgres::{PgRuntimeSettingsRepository, RuntimeSettingsRepository};
    PgRuntimeSettingsRepository::new(database.pool.clone())
        .update_runtime_settings(super::runtime_settings::settings_with_margin(1_800))
        .await
        .unwrap();
    assert_eq!(
        repository.load_turn_state_probe_policy().await.unwrap(),
        policy
    );
    let before: (i64, i64) = sqlx::query_as(
        "select config_revision, refresh_margin_seconds from runtime_settings where id = 1",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    let error = proxies
        .delete(&proxy.id, proxy.revision, &context)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), AdminStoreErrorKind::Conflict);
    let missing = TurnStateProbePolicy {
        proxy_ids: vec!["missing-proxy".to_owned()],
        ..policy.clone()
    };
    assert!(
        repository
            .update_turn_state_probe_policy(
                missing,
                audit(
                    "policy-missing",
                    "turn_state_probe_policy.update",
                    "runtime_settings"
                )
            )
            .await
            .is_err()
    );
    let after: (i64, i64) = sqlx::query_as(
        "select config_revision, refresh_margin_seconds from runtime_settings where id = 1",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        repository.load_turn_state_probe_policy().await.unwrap(),
        policy
    );
    repository
        .update_turn_state_probe_policy(
            TurnStateProbePolicy::default(),
            audit(
                "policy-clear",
                "turn_state_probe_policy.update",
                "runtime_settings",
            ),
        )
        .await
        .unwrap();
    proxies
        .delete(&proxy.id, proxy.revision, &context)
        .await
        .unwrap();
    database.close().await;
}

#[tokio::test]
async fn turn_state_policy_concurrent_save_and_delete_cannot_leave_dangling_reference() {
    let Some(database) = TestDatabase::create("turn_state_policy_race").await else {
        return;
    };
    let repository = PgControlPlaneRepository::new(database.pool.clone());
    let proxies = PgProxyRepository::new(database.pool.clone());
    let context = MutationContext {
        actor: MutationActor::System,
        request_id: "policy-race".to_owned(),
    };
    let proxy = proxies
        .create(
            NewProxy {
                name: "Race proxy".to_owned(),
                proxy: OutboundProxy::parse("http://127.0.0.1:18082").unwrap(),
                location: None,
            },
            &context,
        )
        .await
        .unwrap()
        .record;
    let policy = TurnStateProbePolicy {
        mode: TurnStateProxyMode::Fixed,
        proxy_ids: vec![proxy.id.clone()],
        candidate_limit: 1,
        ..Default::default()
    };
    let (saved, deleted) = tokio::join!(
        repository.update_turn_state_probe_policy(
            policy.clone(),
            audit(
                "policy-race-save",
                "turn_state_probe_policy.update",
                "runtime_settings"
            )
        ),
        proxies.delete(&proxy.id, proxy.revision, &context),
    );
    assert_ne!(saved.is_ok(), deleted.is_ok());
    let loaded = repository.load_turn_state_probe_policy().await.unwrap();
    if saved.is_ok() {
        assert_eq!(loaded, policy);
        assert!(proxies.get(&proxy.id).await.is_ok());
    } else {
        assert_eq!(loaded, TurnStateProbePolicy::default());
        assert!(proxies.get(&proxy.id).await.is_err());
    }
    database.close().await;
}

#[tokio::test]
async fn verified_policy_migration_upgrades_once_and_preserves_later_edits() {
    let Some(database) = TestDatabase::create("verified_policy_upgrade").await else {
        return;
    };
    sqlx::query("update runtime_settings set turn_state_probe_policy_json = $1")
        .bind(serde_json::json!({"schemaVersion":2,"manualEnabled":false,"automaticEnabled":false,"mode":"smart","proxyIds":[],"candidateLimit":2,"schedule":{"scanIntervalSeconds":30,"budgetLimit":3},"request":{"inputText":"old"}}))
        .execute(&database.pool).await.unwrap();
    let migration = include_str!("../../../../migrations/0019_verified_turn_state.sql");
    sqlx::raw_sql(migration)
        .execute(&database.pool)
        .await
        .unwrap();
    let repository = PgControlPlaneRepository::new(database.pool.clone());
    let upgraded = repository.load_turn_state_probe_policy().await.unwrap();
    assert_eq!(
        upgraded.verification.mode,
        gateway_admin::model::turn_state::TurnStateVerificationMode::MintAndValidate
    );
    assert_eq!(upgraded.verification.reuse_count, 3);
    assert_eq!(upgraded.schedule.budget_limit, 12);
    assert_eq!(upgraded.schedule.scan_interval_seconds, 30);
    assert!(!upgraded.manual_enabled && !upgraded.automatic_enabled);
    assert_eq!(upgraded.candidate_limit, 2);
    let mut edited = upgraded.clone();
    edited.verification.reuse_count = 2;
    sqlx::query("update runtime_settings set turn_state_probe_policy_json = $1")
        .bind(serde_json::to_value(&edited).unwrap())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::raw_sql(migration)
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        repository.load_turn_state_probe_policy().await.unwrap(),
        edited
    );
    database.close().await;
}
