use url::Url;

use provider_openai::credential::{CodexCookiePolicy, CookiePolicyError};

fn policy() -> CodexCookiePolicy {
    CodexCookiePolicy::new(["session"], ["chatgpt.com"]).expect("valid policy")
}

#[test]
fn capture_should_reject_parent_public_suffix_outside_allowlist() {
    let error = policy()
        .validate_capture(
            &Url::parse("https://chatgpt.com/backend-api").expect("valid URL"),
            Some("com"),
            "session",
            "/",
        )
        .err()
        .expect("public suffix must be rejected");

    assert_eq!(error, CookiePolicyError::InvalidScope);
}

#[test]
fn replay_should_respect_host_only_cookie_scope() {
    let policy = policy();

    assert!(!policy.may_replay(
        &Url::parse("https://api.chatgpt.com/backend-api").expect("valid URL"),
        "session",
        "chatgpt.com",
        "/",
        true,
        true,
    ));
}

#[test]
fn replay_should_respect_secure_cookie_attribute() {
    let policy = policy();

    assert!(!policy.may_replay(
        &Url::parse("http://chatgpt.com/backend-api").expect("valid URL"),
        "session",
        "chatgpt.com",
        "/",
        false,
        true,
    ));
}

#[test]
fn official_policy_accepts_cloudflare_infrastructure_cookies_without_broadening_domains() {
    let policy = CodexCookiePolicy::official().expect("official policy");
    let origin = Url::parse("https://chatgpt.com/backend-api").expect("valid URL");
    for name in [
        "__cf_bm",
        "__cflb",
        "__cfruid",
        "__cfseq",
        "__cfwaitingroom",
        "__oailb",
        "_cfuvid",
        "cf_clearance",
        "cf_ob_info",
        "cf_use_ob",
        "cf_chl_abc",
    ] {
        assert!(
            policy.validate_capture(&origin, None, name, "/").is_ok(),
            "{name}"
        );
    }
    assert!(matches!(
        policy.validate_capture(&origin, None, "cf_chl", "/"),
        Err(CookiePolicyError::NameNotAllowed)
    ));
    assert!(matches!(
        policy.validate_capture(
            &Url::parse("https://evil.example").expect("valid URL"),
            None,
            "__cf_bm",
            "/",
        ),
        Err(CookiePolicyError::InvalidOrigin)
    ));
    assert!(matches!(
        policy.validate_capture(
            &Url::parse("https://api.openai.com/v1").expect("valid URL"),
            None,
            "__cf_bm",
            "/",
        ),
        Err(CookiePolicyError::InvalidOrigin)
    ));
    assert!(!policy.may_replay(
        &Url::parse("https://api.openai.com/v1").expect("valid URL"),
        "__cf_bm",
        "openai.com",
        "/",
        false,
        true,
    ));
}
