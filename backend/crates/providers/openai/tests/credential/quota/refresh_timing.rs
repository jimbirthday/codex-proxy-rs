//! 通过 quota 服务验证周期复核与 reset 宽限期，不依赖内部调度状态。

use std::time::Duration;

use super::*;

async fn mount_usage(server: &MockServer, value: serde_json::Value) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(value))
        .mount(server)
        .await;
}

async fn wait_for_reset_grace(reset: i64) {
    let due = SystemTime::UNIX_EPOCH + Duration::from_secs((reset + 120) as u64);
    if let Ok(remaining) = due.duration_since(SystemTime::now()) {
        tokio::time::sleep(remaining + Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn reset_grace_bypasses_periodic_throttle_once_then_allows_the_next_window() {
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_quota_timing").await;
    let account = store.account("acct_quota_timing").expect("account");
    let server = MockServer::start().await;
    let service = quota_service_with_base_url(&store, reqwest::Client::new(), server.uri());
    // 使用接近宽限期终点的真实窗口，在数秒内走完两个窗口的调度与落库路径。
    let short_reset = Utc::now().timestamp() - 115;
    let week_reset = short_reset + 3;
    let usage = |short_used, short_reset, week_used, week_reset| {
        json!({"rate_limit": {
            "allowed": false,
            "primary_window": {
                "used_percent": short_used, "reset_at": short_reset, "limit_window_seconds": 18_000,
            },
            "secondary_window": {
                "used_percent": week_used, "reset_at": week_reset, "limit_window_seconds": 604_800,
            },
        }})
    };
    mount_usage(&server, usage(100, short_reset, 100, week_reset)).await;
    service
        .refresh_account(account.id())
        .await
        .expect("seed exhaustion");
    service.synchronize().await.expect("initial periodic check");
    let requests = server.received_requests().await.expect("requests").len();

    assert!(
        Utc::now().timestamp() < short_reset + 120,
        "fixture must precede grace deadline"
    );
    service.synchronize().await.expect("before grace deadline");
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        requests
    );

    mount_usage(&server, usage(0, short_reset + 18_000, 100, week_reset)).await;
    wait_for_reset_grace(short_reset).await;
    let partial = service.synchronize().await.expect("short reset check");
    assert_eq!(partial.exhausted, 1);
    assert_eq!(
        store
            .account("acct_quota_timing")
            .expect("account")
            .quota()
            .reset_at(),
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(week_reset as u64))
    );
    service
        .synchronize()
        .await
        .expect("next scan before weekly grace deadline");
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);

    // 周窗口的到期复核仍未恢复时，也不能每轮扫描重复请求。
    wait_for_reset_grace(week_reset).await;
    assert_eq!(
        service
            .synchronize()
            .await
            .expect("weekly reset check")
            .exhausted,
        1
    );
    service
        .synchronize()
        .await
        .expect("repeat scan after weekly check");
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
}

#[tokio::test]
async fn periodic_checks_continue_when_reset_is_unknown_or_far_in_the_future() {
    for reset in [None, Some(Utc::now().timestamp() + 604_800)] {
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_quota_periodic").await;
        let account = store.account("acct_quota_periodic").expect("account");
        let server = MockServer::start().await;
        let service = quota_service_with_base_url(&store, reqwest::Client::new(), server.uri());
        let mut usage = json!({"rate_limit": {
            "allowed": false,
            "primary_window": {"used_percent": 100, "limit_window_seconds": 604_800},
        }});
        if let Some(reset) = reset {
            usage["rate_limit"]["primary_window"]["reset_at"] = json!(reset);
        }
        mount_usage(&server, usage.clone()).await;
        service
            .refresh_account(account.id())
            .await
            .expect("seed exhaustion");
        mount_usage(&server, usage).await;

        assert_eq!(
            service
                .synchronize()
                .await
                .expect("periodic check")
                .exhausted,
            1
        );
        service.synchronize().await.expect("throttled repeat check");
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }
}

#[tokio::test]
async fn allowed_account_with_expired_window_synchronizes_at_reset_grace() {
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_allowed_expired_window").await;
    let account = store
        .account("acct_allowed_expired_window")
        .expect("account");
    let server = MockServer::start().await;
    let service = quota_service_with_base_url(&store, reqwest::Client::new(), server.uri());

    let short_reset = Utc::now().timestamp() - 114;
    let usage = |used: u32, reset: i64| {
        json!({"rate_limit": {
            "allowed": true,
            "primary_window": {
                "used_percent": used,
                "reset_at": reset,
                "limit_window_seconds": 18_000,
            }
        }})
    };

    mount_usage(&server, usage(100, short_reset)).await;
    service
        .refresh_account(account.id())
        .await
        .expect("seed quota with 100% usage");

    assert!(
        Utc::now().timestamp() < short_reset + 120,
        "fixture must precede grace deadline"
    );
    let requests = server.received_requests().await.expect("requests").len();
    service.synchronize().await.expect("initial periodic check");
    service
        .synchronize()
        .await
        .expect("repeat check before grace");
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        requests,
        "正常账号首次复核也必须等待 reset 宽限期"
    );

    // 当到达 reset + 120s 宽限期后，账号虽然处于 allowed 状态，但包含已到期的非零用量窗口，被调度主动同步。
    mount_usage(&server, usage(0, short_reset + 18_000)).await;
    wait_for_reset_grace(short_reset).await;

    let summary = service
        .synchronize()
        .await
        .expect("synchronize expired window");
    assert_eq!(summary.updated, 1);
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);

    let snapshot = service
        .read_account(account.id())
        .await
        .expect("read quota")
        .expect("snapshot");
    assert_eq!(snapshot.windows()[0].used_percent(), Some(0.0));
    service.synchronize().await.expect("skip fresh window");
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn allowed_expired_window_refresh_is_throttled_when_observation_is_unchanged() {
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_allowed_expired_retry").await;
    let account = store
        .account("acct_allowed_expired_retry")
        .expect("account");
    let server = MockServer::start().await;
    let service = quota_service_with_base_url(&store, reqwest::Client::new(), server.uri());
    let usage = json!({"rate_limit": {
        "allowed": true,
        "primary_window": {
            "used_percent": 74,
            "reset_at": Utc::now().timestamp() - 180,
            "limit_window_seconds": 18_000,
        }
    }});
    mount_usage(&server, usage.clone()).await;
    service
        .refresh_account(account.id())
        .await
        .expect("seed quota");
    mount_usage(&server, usage).await;

    assert_eq!(service.synchronize().await.expect("first check").updated, 1);
    service.synchronize().await.expect("throttled repeat check");
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}
