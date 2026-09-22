use super::*;
use gateway_admin::model::turn_state::{TurnStateProbePolicy, TurnStateRuntimeClear};

fn completed(model: &str, cookie: &str, state: char) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .insert_header("x-codex-turn-state", state.to_string().repeat(292))
        .append_header("set-cookie", format!("__cflb={cookie}; Path=/; Secure; Max-Age=3600"))
        .append_header("set-cookie", format!("__oailb={cookie}; Path=/; Secure; Max-Age=3600"))
        .set_body_string(format!("event: response.completed\ndata: {}\n\n", json!({"type":"response.completed", "response":{"id":"resp_verified_account", "model":model,"status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})))
}

#[tokio::test]
async fn defaults_freeze_minted_bundle_and_business_follows_verified_account_proxy() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_verified_account"]).await;
    let account = store.account("acct_verified_account").unwrap();
    let model = upstream_model("gpt-5.4");
    mount_turn_state_sequence(
        &proxy,
        Arc::new(Notify::new()),
        vec![
            completed("gpt-5.4", "minted", 'a'),
            completed("gpt-5.4", "rotated", 'b'),
            completed("gpt-5.4", "rotated", 'c'),
            completed("gpt-5.4", "rotated", 'd'),
            completed("gpt-5.4", "business", 'e'),
            completed("gpt-5.4", "continued", 'f'),
        ],
    )
    .await;
    let result = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &model,
            vec![probe_target("verified_account-proxy", &proxy)],
            TurnStateSource::ManualProbe,
            TurnStateProbePolicy::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        result.active_target_id.as_deref(),
        Some("verified_account-proxy")
    );
    assert_eq!(result.attempts.len(), 4);
    assert!(result.attempts.iter().all(|attempt| attempt.success));
    let mut session = None;
    for request_id in ["req_verified_first", "req_verified_continued"] {
        let payload = ProtocolPayload::json_object(
            "openai",
            json!({"model":"gpt-5.4","input":"continue verified session"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap()
        .with_context(Map::from_iter([("use_websocket".to_owned(), json!(false))]));
        let mut generate = GenerateRequest::from_protocol_payload(payload);
        if let Some(state) = session.take() {
            generate = generate.with_provider_session_state(state);
        }
        let mut stream = bundle
            .core_provider()
            .execute(
                initialized_provider_request(Operation::Generate(generate), account.id().as_str()),
                initialized_attempt_context(request_id, account.id().as_str()),
            )
            .await
            .unwrap();
        while let Some(event) = stream.next().await {
            if let Some(update) = event.unwrap().session_update() {
                session = Some(update.clone());
            }
        }
        assert!(
            session
                .as_ref()
                .unwrap()
                .payload()
                .get("verified_bundle_id")
                .is_some()
        );
    }
    bundle
        .admin_provider()
        .clear_turn_state_runtime(&TurnStateRuntimeClear {
            account_id: Some(account.id().as_str().to_owned()),
            model: None,
            rule_id: None,
            response_headers: true,
            turn_state: true,
        });
    let payload = ProtocolPayload::json_object(
        "openai",
        json!({"model":"gpt-5.4","input":"continue expired session"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap()
    .with_context(Map::from_iter([("use_websocket".to_owned(), json!(false))]));
    let generate = GenerateRequest::from_protocol_payload(payload)
        .with_provider_session_state(session.unwrap());
    let result = bundle
        .core_provider()
        .execute(
            initialized_provider_request(Operation::Generate(generate), account.id().as_str()),
            initialized_attempt_context("req_verified_expired", account.id().as_str()),
        )
        .await;
    assert!(
        matches!(result, Err(error) if error.kind() == gateway_core::error::ProviderErrorKind::ContinuationRecoveryRequired)
    );
    let requests = proxy.received_requests().await.unwrap();
    assert_eq!(requests.len(), 6);
    assert!(!requests[0].headers.contains_key("cookie"));
    assert!(!requests[0].headers.contains_key("x-codex-turn-state"));
    for request in &requests[1..] {
        assert_eq!(
            request.headers.get("x-codex-turn-state").unwrap(),
            &"a".repeat(292)
        );
        assert_eq!(
            request.headers.get("cookie").unwrap(),
            "__cflb=minted; __oailb=minted"
        );
        assert_eq!(
            request
                .headers
                .get("x-openai-internal-codex-responses-lite")
                .unwrap(),
            "true"
        );
    }
    for request in &requests[..4] {
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            value.pointer("/input/0/type"),
            Some(&json!("additional_tools"))
        );
        assert_eq!(
            value.pointer("/reasoning/context"),
            Some(&json!("all_turns"))
        );
        assert_eq!(value.get("parallel_tool_calls"), Some(&json!(false)));
    }
    assert!(base.received_requests().await.unwrap().is_empty());
    assert!(
        store
            .account("acct_verified_account")
            .unwrap()
            .outbound_proxy()
            .is_none()
    );
}

#[tokio::test]
async fn wrong_model_does_not_publish_state_or_cookies() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_failed_verified_account"]).await;
    let account = store.account("acct_failed_verified_account").unwrap();
    mount_turn_state_sequence(
        &proxy,
        Arc::new(Notify::new()),
        vec![
            completed("gpt-5.4", "minted", 'a'),
            completed("wrong-model", "bad", 'b'),
        ],
    )
    .await;
    let result = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-5.4"),
            vec![probe_target("p", &proxy)],
            TurnStateSource::ManualProbe,
            TurnStateProbePolicy::default(),
        )
        .await
        .unwrap();
    assert!(result.active_target_id.is_none());
    assert_eq!(result.attempts.len(), 2);
    assert!(result.attempts[1].message.contains("实际模型"));
    assert!(
        bundle
            .admin_provider()
            .turn_state_snapshot(account.id(), &upstream_model("gpt-5.4"))
            .unwrap()
            .state_expires_at
            .is_none()
    );
    assert_eq!(
        bundle
            .admin_provider()
            .response_header_carry_status()
            .total_entries,
        0
    );
}

#[tokio::test]
async fn late_validation_cannot_restore_cleared_bundle() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_clear_verified_account"]).await;
    let account = store.account("acct_clear_verified_account").unwrap();
    let seen = Arc::new(Notify::new());
    let count = mount_turn_state_sequence(
        &proxy,
        seen.clone(),
        vec![
            completed("gpt-5.4", "minted", 'a'),
            completed("gpt-5.4", "new", 'b').set_delay(Duration::from_millis(200)),
        ],
    )
    .await;
    let admin = bundle.admin_provider();
    let task = tokio::spawn({
        let admin = admin.clone();
        let account = account.clone();
        let target = probe_target("p", &proxy);
        async move {
            admin
                .probe_turn_state(
                    account.id(),
                    &upstream_model("gpt-5.4"),
                    vec![target],
                    TurnStateSource::ManualProbe,
                    TurnStateProbePolicy::default(),
                )
                .await
                .unwrap()
        }
    });
    wait_for_request(&seen, &count, 2).await;
    admin.clear_turn_state_runtime(&TurnStateRuntimeClear {
        account_id: Some(account.id().as_str().to_owned()),
        model: None,
        rule_id: None,
        response_headers: true,
        turn_state: true,
    });
    assert!(task.await.unwrap().active_target_id.is_none());
    assert!(
        admin
            .turn_state_snapshot(account.id(), &upstream_model("gpt-5.4"))
            .is_none()
    );
}

#[tokio::test]
async fn http_200_failed_sse_never_counts_as_success() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_failed_sse"]).await;
    let account = store.account("acct_failed_sse").unwrap();
    mount_turn_state_sequence(&proxy, Arc::new(Notify::new()), vec![completed("gpt-5.4", "minted", 'a').set_body_string("event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.4\"}}\n\nevent: response.failed\ndata: {\"type\":\"response.failed\"}\n\n")]).await;
    let result = bundle
        .admin_provider()
        .probe_turn_state(
            account.id(),
            &upstream_model("gpt-5.4"),
            vec![probe_target("p", &proxy)],
            TurnStateSource::ManualProbe,
            TurnStateProbePolicy::default(),
        )
        .await
        .unwrap();
    assert!(result.active_target_id.is_none());
    assert!(!result.attempts[0].success);
}

#[tokio::test]
async fn draft_uses_its_state_rules_without_changing_the_published_bundle() {
    let base = MockServer::start().await;
    let proxy = MockServer::start().await;
    let (bundle, store) = turn_state_fixture(&base, &["acct_draft_isolation"]).await;
    let account = store.account("acct_draft_isolation").unwrap();
    let model = upstream_model("gpt-5.4");
    mount_turn_state_sequence(
        &proxy,
        Arc::new(Notify::new()),
        vec![
            completed("gpt-5.4", "original", 'a'),
            completed("gpt-5.4", "original", 'a'),
            completed("gpt-5.4", "original", 'a'),
            completed("gpt-5.4", "original", 'a'),
            completed("gpt-5.4", "draft", 'b').insert_header("x-draft-state", "d".repeat(300)),
            completed("gpt-5.4", "draft", 'b'),
            completed("gpt-5.4", "business", 'e'),
        ],
    )
    .await;
    let admin = bundle.admin_provider();
    admin
        .probe_turn_state(
            account.id(),
            &model,
            vec![probe_target("p", &proxy)],
            TurnStateSource::ManualProbe,
            TurnStateProbePolicy::default(),
        )
        .await
        .unwrap();
    let before = admin.turn_state_snapshot(account.id(), &model).unwrap();
    let mut draft = TurnStateProbePolicy::default();
    draft.verification.reuse_count = 1;
    draft.state.accepted_lengths = vec![300];
    draft.state.response_header_names = vec!["x-draft-state".to_owned()];
    let result = admin
        .test_turn_state_policy(
            account.id(),
            &model,
            vec![probe_target("draft", &proxy)],
            draft,
        )
        .await
        .unwrap();
    assert_eq!(result.attempts.len(), 2);
    assert!(result.attempts.iter().all(|attempt| attempt.success));
    assert_eq!(
        before,
        admin.turn_state_snapshot(account.id(), &model).unwrap()
    );
    drain_turn_state_business(
        &bundle,
        account.id().as_str(),
        "gpt-5.4",
        "req_after_draft",
        true,
    )
    .await;
    let requests = proxy.received_requests().await.unwrap();
    assert_eq!(
        requests[5].headers.get("x-codex-turn-state").unwrap(),
        &"d".repeat(300)
    );
    assert_eq!(
        requests[6].headers.get("x-codex-turn-state").unwrap(),
        &"a".repeat(292)
    );
    assert_eq!(
        requests[6].headers.get("cookie").unwrap(),
        "__cflb=original; __oailb=original"
    );
}
