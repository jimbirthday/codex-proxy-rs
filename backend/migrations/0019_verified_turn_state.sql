-- 升级后直接使用铸票与连续验证预设，保留管理员开关和代理目录选择。
-- 只转换旧版本一次；之后页面保存的完整策略不会被重启覆盖。
update runtime_settings
set turn_state_probe_policy_json =
    (turn_state_probe_policy_json - 'request' - 'state' - 'verification' - 'reuseRequest')
    || jsonb_build_object(
        'schemaVersion', 3,
        'schedule', coalesce(turn_state_probe_policy_json->'schedule', '{}'::jsonb)
            || '{"budgetLimit":12,"requestTimeoutSeconds":20}'::jsonb
    )
where coalesce((turn_state_probe_policy_json->>'schemaVersion')::integer, 0) < 3;

alter table runtime_settings alter column turn_state_probe_policy_json set default
    '{"schemaVersion":3,"manualEnabled":true,"automaticEnabled":true,"mode":"smart","proxyIds":[],"candidateLimit":3}'::jsonb;
