use gateway_admin::model::turn_state::*;

#[test]
fn legacy_policy_json_receives_v2_defaults() {
    let policy: TurnStateProbePolicy = serde_json::from_value(serde_json::json!({
        "manualEnabled": true,
        "automaticEnabled": true,
        "mode": "smart",
        "proxyIds": [],
        "candidateLimit": 3
    }))
    .expect("legacy policy");
    assert_eq!(policy.schedule.max_concurrency, 2);
    assert_eq!(policy.state.accepted_lengths, vec![292, 332]);
    assert!(policy.response_header_carry.rules.is_empty());
    policy.validate().expect("defaulted policy is valid");
}

#[test]
fn generic_rule_can_override_managed_headers() {
    let mut policy = TurnStateProbePolicy::default();
    policy
        .response_header_carry
        .rules
        .push(ResponseHeaderCarryRule {
            id: "bad_rule".to_owned(),
            name: "Bad".to_owned(),
            enabled: true,
            capture_enabled: true,
            injection_enabled: true,
            clear_on_disable: false,
            sources: vec![ResponseHeaderCarrySource::BusinessResponse],
            source_header: "x-session".to_owned(),
            target_header: "x-codex-turn-state".to_owned(),
            transform: ResponseHeaderTransform::Direct,
            value_selection: ResponseHeaderValueSelection::Last,
            merge_mode: ResponseHeaderMergeMode::Replace,
            scope: ResponseHeaderCarryScope::Account,
            account_ids: Vec::new(),
            models: Vec::new(),
            ttl_seconds: 60,
            missing_behavior: ResponseHeaderMissingBehavior::Keep,
            capture_status_min: 200,
            capture_status_max: 299,
            invalidation_statuses: Vec::new(),
            max_value_bytes: 8_192,
            max_values: 1,
        });
    policy
        .validate()
        .expect("管理员规则允许覆盖系统管理的请求头");
}

#[test]
fn cookie_mapping_supports_direct_and_dedicated_transforms() {
    let mut policy = TurnStateProbePolicy::default();
    let mut rule = ResponseHeaderCarryRule {
        id: "cookie_bundle".to_owned(),
        name: "Cookie bundle".to_owned(),
        enabled: true,
        capture_enabled: true,
        injection_enabled: true,
        clear_on_disable: false,
        sources: vec![ResponseHeaderCarrySource::BusinessResponse],
        source_header: "set-cookie".to_owned(),
        target_header: "cookie".to_owned(),
        transform: ResponseHeaderTransform::Direct,
        value_selection: ResponseHeaderValueSelection::All,
        merge_mode: ResponseHeaderMergeMode::Replace,
        scope: ResponseHeaderCarryScope::AccountModel,
        account_ids: Vec::new(),
        models: Vec::new(),
        ttl_seconds: 240,
        missing_behavior: ResponseHeaderMissingBehavior::Keep,
        capture_status_min: 200,
        capture_status_max: 299,
        invalidation_statuses: vec![401, 403],
        max_value_bytes: 8_192,
        max_values: 16,
    };
    policy.response_header_carry.rules.push(rule.clone());
    policy.validate().expect("direct cookie mapping");

    rule.transform = ResponseHeaderTransform::SetCookieToCookie;
    policy.response_header_carry.rules = vec![rule];
    policy.validate().expect("dedicated cookie mapping");
}
