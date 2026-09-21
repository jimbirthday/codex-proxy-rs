use super::{TestDatabase, provider_accounts::audit};
use gateway_admin::{
    model::{
        MutationActor, MutationContext,
        proxies::NewProxy,
        turn_state::{TurnStateProbePolicy, TurnStateProxyMode},
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
