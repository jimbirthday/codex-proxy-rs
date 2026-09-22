use super::*;
use crate::transport::accept_codex_test_websocket_with;
use futures::SinkExt as _;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

async fn prewarm_fixture(base_url: String) -> provider_openai::ProviderBundle {
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_prewarm".to_owned(),
            name: "prewarm".to_owned(),
            secret: secret("synthetic-prewarm-access"),
            verified_account: profile("chatgpt-prewarm"),
            next_refresh_at: Some(Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    let mut config = valid_config();
    config.config.api.base_url = base_url;
    let bundle = provider_openai::initialize(
        config.config,
        provider_ports_with_catalog_and_leases(
            store,
            Arc::new(TestOAuthPending::default()),
            Arc::new(TestCatalogCache::default()),
            Arc::new(TestLeaseCoordinator::default()),
        ),
    )
    .await
    .unwrap();
    bundle
        .admin_provider()
        .apply_turn_state_probe_policy(acquire_only_policy());
    bundle
}

#[tokio::test]
async fn sends_prewarm_before_reading_and_caches_handshake_or_metadata_state() {
    for handshake_state in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bundle = prewarm_fixture(format!("http://{}", listener.local_addr().unwrap())).await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_codex_test_websocket_with(stream, |request, response| {
                assert_eq!(request.uri().path(), "/codex/responses");
                if handshake_state {
                    response
                        .headers_mut()
                        .insert("x-codex-turn-state", "a".repeat(292).parse().unwrap());
                }
            })
            .await;
            // 服务端收到预热正文前不发送任何事件，覆盖仅握手就开始等响应的回归。
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                panic!("expected response.create text frame");
            };
            let payload: Value = serde_json::from_str(text.as_ref()).unwrap();
            assert_eq!(payload["type"], "response.create");
            assert_eq!(payload["model"], "gpt-5.6-sol");
            assert_eq!(payload["generate"], false);
            assert_eq!(payload["store"], false);
            assert_eq!(payload["parallel_tool_calls"], false);
            assert_eq!(payload["reasoning"]["context"], "all_turns");
            if handshake_state {
                assert_eq!(payload["instructions"], "Custom warmup");
                assert_eq!(payload["input"], json!([]));
            } else {
                assert!(!payload["input"].as_array().unwrap().is_empty());
            }
            let metadata = json!({
                "type": "response.metadata",
                "headers": {"x-codex-turn-state": "b".repeat(292)}
            })
            .to_string();
            for frame in [
                metadata.clone(),
                metadata,
                json!({"type": "response.completed", "response": {"output": []}}).to_string(),
            ] {
                socket.send(Message::Text(frame.into())).await.unwrap();
            }
            assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
            text.as_bytes().to_vec()
        });
        let account = ProviderAccountId::new("acct_prewarm").unwrap();
        let model = upstream_model("gpt-5.6-sol");
        let custom = handshake_state.then(|| {
            json!({
                "type": "ignored",
                "model": "other-model",
                "generate": true,
                "store": true,
                "instructions": "Custom warmup",
                "input": []
            })
            .as_object()
            .unwrap()
            .clone()
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            bundle
                .admin_provider()
                .websocket_prewarm_probe(&account, &model, None, 2, custom),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.exchange.error, None);
        assert_eq!(result.exchange.status_code, Some(101));
        assert_eq!(result.exchange.turn_state_length, Some(292));
        assert!(result.exchange.turn_state_stored);
        assert_eq!(result.exchange.request.body, server.await.unwrap());
        let states = result
            .exchange
            .response_headers
            .iter()
            .filter(|header| header.name == "x-codex-turn-state")
            .collect::<Vec<_>>();
        assert_eq!(states.len(), 1);
        let expected = if handshake_state { "a" } else { "b" }.repeat(292);
        assert_eq!(states[0].value, expected.as_bytes());
        let frames = std::str::from_utf8(&result.body)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2]["type"], "response.completed");
        let snapshot = bundle
            .admin_provider()
            .turn_state_snapshot(&account, &model)
            .unwrap();
        assert!(snapshot.state_captured_at.is_some());
        assert_eq!(
            snapshot.state_source,
            Some(TurnStateSource::UpstreamResponse)
        );
    }
}

#[tokio::test]
async fn silent_upstream_times_out_after_receiving_prewarm_without_caching_state() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bundle = prewarm_fixture(format!("http://{}", listener.local_addr().unwrap())).await;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_codex_test_websocket_with(stream, |_, _| {}).await;
        let first = socket.next().await;
        // 保持连接但不回业务帧，客户端必须按探测截止时间退出。
        let _ = socket.next().await;
        first
    });
    let account = ProviderAccountId::new("acct_prewarm").unwrap();
    let model = upstream_model("gpt-5.6-sol");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        bundle
            .admin_provider()
            .websocket_prewarm_probe(&account, &model, None, 1, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.exchange.status_code, Some(101));
    assert_eq!(result.exchange.error.as_deref(), Some("预热读取超时"));
    assert!(result.body.is_empty());
    assert!(!result.exchange.turn_state_stored);
    assert!(matches!(server.await.unwrap(), Some(Ok(Message::Text(_)))));
}

#[tokio::test]
async fn follow_up_stays_prewarm_and_upgrades_state_from_binary_completion() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bundle = prewarm_fixture(format!("http://{}", listener.local_addr().unwrap())).await;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_codex_test_websocket_with(stream, |_, response| {
            response
                .headers_mut()
                .insert("x-codex-turn-state", "short".parse().unwrap());
        })
        .await;
        assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        socket
            .send(Message::Text(
                json!({"type":"response.completed","response":{"id":"resp_warmup"}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
            panic!("expected follow-up prewarm");
        };
        let payload: Value = serde_json::from_str(text.as_ref()).unwrap();
        assert_eq!(payload["generate"], false);
        assert_eq!(payload["store"], false);
        assert_eq!(payload["parallel_tool_calls"], false);
        assert_eq!(payload["previous_response_id"], "resp_warmup");
        assert_eq!(payload["model"], "gpt-5.6-sol");
        assert_eq!(payload["instructions"], "Custom warmup");
        assert_eq!(payload["input"], json!([]));
        assert_eq!(payload["reasoning"]["context"], "all_turns");
        assert_eq!(payload["client_metadata"]["custom"], "preserved");
        assert_eq!(payload["client_metadata"]["x-codex-turn-state"], "short");
        socket.send(Message::Binary(json!({"type":"response.completed","response":{"id":"resp_followup","current_turn_state":"b".repeat(292)}}).to_string().into_bytes().into())).await.unwrap();
        assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
    });
    let result = bundle.admin_provider().websocket_prewarm_probe(
        &ProviderAccountId::new("acct_prewarm").unwrap(),
        &upstream_model("gpt-5.6-sol"), None, 2,
        Some(json!({"instructions":"Custom warmup","input":"hello","client_metadata":{"custom":"preserved"}}).as_object().unwrap().clone()),
    ).await.unwrap();
    assert_eq!(result.exchange.error, None);
    assert!(result.exchange.turn_state_stored);
    assert_eq!(result.exchange.turn_state_length, Some(292));
    // 响应预览只包含上游帧，不混入本地发送的请求和分隔文字。
    let frames = std::str::from_utf8(&result.body)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2);
    assert!(
        frames
            .iter()
            .all(|frame| frame["type"] == "response.completed")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn failed_terminal_frames_report_error_without_follow_up() {
    for binary in [false, true] {
        for event in ["error", "response.failed", "response.incomplete"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bundle =
                prewarm_fixture(format!("http://{}", listener.local_addr().unwrap())).await;
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = accept_codex_test_websocket_with(stream, |_, _| {}).await;
                assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
                let frame =
                    json!({"type":event,"error":{"message":"synthetic failure"}}).to_string();
                let message = if binary {
                    Message::Binary(frame.into_bytes().into())
                } else {
                    Message::Text(frame.into())
                };
                socket.send(message).await.unwrap();
                assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
            });
            let result = bundle
                .admin_provider()
                .websocket_prewarm_probe(
                    &ProviderAccountId::new("acct_prewarm").unwrap(),
                    &upstream_model("gpt-5.6-sol"),
                    None,
                    2,
                    None,
                )
                .await
                .unwrap();
            let error = result.exchange.error.unwrap();
            assert!(
                error.contains(event) && error.contains("synthetic failure"),
                "{error}"
            );
            assert!(!result.exchange.turn_state_stored);
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn missing_response_id_or_closed_connection_does_not_start_follow_up() {
    for close in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bundle = prewarm_fixture(format!("http://{}", listener.local_addr().unwrap())).await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_codex_test_websocket_with(stream, |_, _| {}).await;
            assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
            if close {
                socket.send(Message::Close(None)).await.unwrap();
            } else {
                socket
                    .send(Message::Text(
                        json!({"type":"response.completed","response":{}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
            }
        });
        let result = bundle
            .admin_provider()
            .websocket_prewarm_probe(
                &ProviderAccountId::new("acct_prewarm").unwrap(),
                &upstream_model("gpt-5.6-sol"),
                None,
                2,
                None,
            )
            .await
            .unwrap();
        if close {
            assert_eq!(
                result.exchange.error.as_deref(),
                Some("预热完成前连接已关闭")
            );
        } else {
            assert_eq!(result.exchange.error, None);
        }
        assert!(!result.exchange.turn_state_stored);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn oversized_frame_stops_capture_at_limit_without_follow_up() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bundle = prewarm_fixture(format!("http://{}", listener.local_addr().unwrap())).await;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_codex_test_websocket_with(stream, |_, _| {}).await;
        assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        socket
            .send(Message::Text("x".repeat(1024 * 1024 + 1).into()))
            .await
            .unwrap();
        assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
    });
    let result = bundle
        .admin_provider()
        .websocket_prewarm_probe(
            &ProviderAccountId::new("acct_prewarm").unwrap(),
            &upstream_model("gpt-5.6-sol"),
            None,
            2,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.body.len(), 1024 * 1024);
    assert_eq!(
        result.exchange.error.as_deref(),
        Some("预热正文超过 1 MiB，已截断")
    );
    server.await.unwrap();
}
