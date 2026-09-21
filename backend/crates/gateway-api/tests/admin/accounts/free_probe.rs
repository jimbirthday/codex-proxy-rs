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
