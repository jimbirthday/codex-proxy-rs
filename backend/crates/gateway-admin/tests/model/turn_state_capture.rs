use gateway_admin::model::turn_state_capture::is_sensitive_probe_header;

#[test]
fn sensitive_header_detection_is_conservative() {
    for name in [
        "Authorization",
        "Set-Cookie",
        "X-Codex-Turn-State",
        "x-service-token-id",
        "x-client-secret-version",
        "x-api-key-id",
    ] {
        assert!(is_sensitive_probe_header(name), "{name}");
    }
    assert!(!is_sensitive_probe_header("content-type"));
    assert!(!is_sensitive_probe_header("x-request-id"));
}
