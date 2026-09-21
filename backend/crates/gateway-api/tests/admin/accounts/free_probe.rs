use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use gateway_api::admin;
use tower::ServiceExt as _;

use super::super::{AdminTestFixture, AdminTestState};

#[tokio::test]
async fn free_probe_requires_admin_and_rejects_invalid_raw_bytes() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (authenticated, body, expected) in [
        (false, serde_json::json!({}), StatusCode::UNAUTHORIZED),
        (
            true,
            serde_json::json!({
                "method": "POST", "url": "http://localhost/test", "headers": [],
                "bodyBase64": "invalid!", "timeoutSeconds": 5,
            }),
            StatusCode::BAD_REQUEST,
        ),
        (
            true,
            serde_json::json!({
                "method": "POST", "url": "http://localhost/test", "headers": [],
                "bodyBase64": "", "timeoutSeconds": 5,
                "proxyId": "saved", "proxyUrl": "http://localhost:8080",
            }),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/admin/accounts/free-probe")
            .header("content-type", "application/json")
            .header("x-request-id", "req_free_probe");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let response = admin::router::<AdminTestState>()
            .with_state(fixture.state())
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
}

struct StreamingProbe(std::sync::Arc<std::sync::atomic::AtomicBool>);
struct ProbeBody;
struct StreamGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl gateway_admin::ports::proxy::HttpProbeBody for ProbeBody {
    fn byte_length(&self) -> u64 {
        7
    }
    fn finished_at(&self) -> Option<std::time::Instant> {
        None
    }
    async fn read(
        &self,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, gateway_admin::model::AdminError> {
        Ok(b"partial"[offset as usize..(offset as usize + length).min(7)].to_vec())
    }
}
#[async_trait::async_trait]
impl gateway_admin::ports::proxy::ProxyProbe for StreamingProbe {
    async fn test(
        &self,
        _: &gateway_core::account::OutboundProxy,
    ) -> gateway_admin::model::proxies::ProxyTestResult {
        panic!("unexpected proxy test")
    }
    async fn send_http(
        &self,
        _: Option<&gateway_core::account::OutboundProxy>,
        _: gateway_admin::model::proxies::HttpProbeRequest,
    ) -> Result<gateway_admin::model::proxies::HttpProbeSession, gateway_admin::model::AdminError>
    {
        use futures::StreamExt as _;
        // 准备过程超过全局响应超时，仍由已经建立的 SSE 生命周期承载。
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let guard = StreamGuard(std::sync::Arc::clone(&self.0));
        Ok(gateway_admin::model::proxies::HttpProbeSession {
            body: std::sync::Arc::new(ProbeBody),
            events: futures::stream::unfold(guard, |guard| async move {
                std::future::pending::<()>().await;
                Some((
                    gateway_admin::model::proxies::HttpProbeEvent::Progress {
                        received_bytes: 7,
                        preview: Vec::new(),
                    },
                    guard,
                ))
            })
            .boxed(),
        })
    }
}

#[tokio::test]
async fn free_probe_stream_survives_handler_timeout_downloads_partial_body_and_cancels_on_drop() {
    use futures::StreamExt as _;
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fixture = AdminTestFixture::with_http_probe(std::sync::Arc::new(StreamingProbe(
        std::sync::Arc::clone(&dropped),
    )))
    .await;
    fixture.auth.insert_session("valid-session");
    let app = admin::router::<AdminTestState>()
        .with_state(fixture.state())
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_millis(50),
        ));
    let response = app.clone().oneshot(Request::builder().method("POST").uri("/api/admin/accounts/free-probe")
        .header("x-request-id", "req_stream_probe").header(header::COOKIE, "cpr_session=valid-session").header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::json!({"method":"GET","url":"http://localhost/","headers":[],"bodyBase64":"","timeoutSeconds":0}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut stream = response.into_body().into_data_stream();
    assert!(
        String::from_utf8(stream.next().await.unwrap().unwrap().to_vec())
            .unwrap()
            .contains("event: connecting")
    );
    let prepared = String::from_utf8(stream.next().await.unwrap().unwrap().to_vec()).unwrap();
    let data = prepared
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(data).unwrap();
    let id = value["id"].as_str().unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/admin/accounts/free-probe/body?id={id}"))
                .header("x-request-id", "req_download_probe")
                .header(header::COOKIE, "cpr_session=valid-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "7");
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 100)
            .await
            .unwrap(),
        "partial"
    );
    drop(stream);
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
}
