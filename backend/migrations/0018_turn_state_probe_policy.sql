alter table runtime_settings
    add column turn_state_probe_policy_json jsonb not null default
        '{"manualEnabled":true,"automaticEnabled":true,"mode":"smart","proxyIds":[],"candidateLimit":3}'::jsonb,
    add constraint runtime_settings_turn_state_probe_policy_ck check (
        jsonb_typeof(turn_state_probe_policy_json) = 'object'
        and octet_length(turn_state_probe_policy_json::text) <= 32768
    );
