//! Codex turn state 的管理态投影；不包含 state 原文。

use chrono::{DateTime, Utc};
use gateway_core::{
    account::{OutboundProxy, ProviderAccountId},
    routing::UpstreamModelId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStateSource {
    UpstreamResponse,
    ManualProbe,
    AutomaticRenewal,
}

impl TurnStateSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpstreamResponse => "upstream_response",
            Self::ManualProbe => "manual_probe",
            Self::AutomaticRenewal => "automatic_renewal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateProbeSubject {
    pub account_id: ProviderAccountId,
    pub model: UpstreamModelId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateProbeTarget {
    pub id: String,
    pub label: String,
    pub proxy: Option<OutboundProxy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateProbeAttempt {
    pub exchange_id: Option<String>,
    pub target_id: String,
    pub target_label: String,
    pub success: bool,
    pub status_code: Option<u16>,
    pub latency_ms: u64,
    pub state_acquired: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateProbeResult {
    pub account_id: String,
    pub model: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub trigger: TurnStateSource,
    pub active_target_id: Option<String>,
    pub state_expires_at: Option<DateTime<Utc>>,
    pub attempts: Vec<TurnStateProbeAttempt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateSnapshot {
    pub account_id: String,
    pub model: String,
    pub state_captured_at: Option<DateTime<Utc>>,
    pub state_first_applied_at: Option<DateTime<Utc>>,
    pub state_expires_at: Option<DateTime<Utc>>,
    pub next_rotation_at: Option<DateTime<Utc>>,
    pub state_source: Option<TurnStateSource>,
    pub probe_history: Vec<TurnStateProbeResult>,
    pub invalidated_at: Option<DateTime<Utc>>,
    pub invalidation_reason: Option<String>,
}

/// 账号总览使用的单个账号/模型状态摘要；不包含 state 原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateOverviewEntry {
    pub account_id: String,
    pub model: String,
    pub state_available: bool,
    pub state_captured_at: Option<DateTime<Utc>>,
    pub state_first_applied_at: Option<DateTime<Utc>>,
    pub state_expires_at: Option<DateTime<Utc>>,
    pub next_rotation_at: Option<DateTime<Utc>>,
    pub state_source: Option<TurnStateSource>,
    pub latest_probe_at: Option<DateTime<Utc>>,
    pub latest_probe_succeeded: bool,
    pub latest_probe_active_target_id: Option<String>,
    pub latest_probe_active_target_label: Option<String>,
    pub latest_probe_attempt_count: usize,
    pub invalidated_at: Option<DateTime<Utc>>,
    pub invalidation_reason: Option<String>,
}

/// 探测策略只引用代理目录 ID，不复制代理地址或认证信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStateProxyMode {
    Smart,
    Fixed,
    Pool,
    Random,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct TurnStateProbePolicy {
    pub schema_version: u8,
    pub verification: TurnStateVerification,
    pub reuse_request: TurnStateProbeRequest,
    pub manual_enabled: bool,
    pub automatic_enabled: bool,
    pub mode: TurnStateProxyMode,
    /// 随机模式为空时使用完整目录；固定及代理池模式必须显式选择。
    pub proxy_ids: Vec<String>,
    pub candidate_limit: u8,
    pub schedule: TurnStateProbeSchedule,
    pub request: TurnStateProbeRequest,
    pub state: TurnStatePolicy,
    pub response_header_carry: ResponseHeaderCarryPolicy,
}

impl Default for TurnStateProbePolicy {
    fn default() -> Self {
        Self {
            schema_version: 3,
            verification: TurnStateVerification::default(),
            reuse_request: TurnStateProbeRequest::reuse_default(),
            manual_enabled: true,
            automatic_enabled: true,
            mode: TurnStateProxyMode::Smart,
            proxy_ids: Vec::new(),
            candidate_limit: 3,
            schedule: TurnStateProbeSchedule::default(),
            request: TurnStateProbeRequest::default(),
            state: TurnStatePolicy::default(),
            response_header_carry: ResponseHeaderCarryPolicy::default(),
        }
    }
}

/// 后台扫描与单账号探测节流。单位显式写入字段名，避免管理端误用。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct TurnStateProbeSchedule {
    pub scan_interval_seconds: u64,
    pub round_min_interval_seconds: u64,
    pub request_spacing_milliseconds: u64,
    pub request_timeout_seconds: u64,
    pub activity_window_seconds: u64,
    pub budget_window_seconds: u64,
    pub budget_limit: u16,
    pub max_concurrency: u8,
    pub retry_initial_seconds: u64,
    pub retry_max_seconds: u64,
    pub proxy_cooldown_initial_seconds: u64,
    pub proxy_cooldown_max_seconds: u64,
}

impl Default for TurnStateProbeSchedule {
    fn default() -> Self {
        Self {
            scan_interval_seconds: 10,
            round_min_interval_seconds: 10,
            request_spacing_milliseconds: 100,
            request_timeout_seconds: 20,
            activity_window_seconds: 60 * 60,
            budget_window_seconds: 60,
            budget_limit: 12,
            max_concurrency: 2,
            retry_initial_seconds: 15,
            retry_max_seconds: 120,
            proxy_cooldown_initial_seconds: 30,
            proxy_cooldown_max_seconds: 120,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStateProbeCompression {
    None,
    Zstd,
}

/// 探测请求模板。`extra_body` 只补充未由结构化字段拥有的顶层字段。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct TurnStateProbeRequest {
    /// 完整请求体模板优先于便捷字段；字符串 $model 替换为本轮上游模型。
    pub body: Option<serde_json::Map<String, serde_json::Value>>,
    pub compression: TurnStateProbeCompression,
    pub compression_level: i32,
    pub instructions: Option<String>,
    pub input_text: String,
    pub reasoning_effort: Option<String>,
    pub stream: bool,
    pub store: bool,
    pub parallel_tool_calls: Option<bool>,
    pub include: Vec<String>,
    pub service_tier: Option<String>,
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    pub extra_headers: Vec<TurnStateProbeRequestHeader>,
    pub max_response_body_bytes: u32,
}

impl Default for TurnStateProbeRequest {
    fn default() -> Self {
        Self {
            body: Some(default_mint_body()),
            compression: TurnStateProbeCompression::None,
            compression_level: 3,
            instructions: None,
            input_text: "Reply with exactly OK.".to_owned(),
            reasoning_effort: None,
            stream: true,
            store: false,
            parallel_tool_calls: None,
            include: Vec::new(),
            service_tier: None,
            extra_body: serde_json::Map::new(),
            extra_headers: [
                ("Originator", "codex_cli_rs"),
                ("User-Agent", "codex_cli_rs/0.155.0"),
                ("Version", "0.155.0"),
                ("OpenAI-Beta", "responses_websockets=2026-02-06"),
                ("X-OpenAI-Internal-Codex-Responses-Lite", "true"),
            ]
            .into_iter()
            .map(|(name, value)| TurnStateProbeRequestHeader {
                name: name.to_owned(),
                value: value.to_owned(),
            })
            .collect(),
            max_response_body_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnStateProbeRequestHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct TurnStatePolicy {
    pub capture_business_responses: bool,
    pub capture_probe_responses: bool,
    pub injection_enabled: bool,
    pub response_header_names: Vec<String>,
    pub response_json_pointers: Vec<String>,
    pub accepted_lengths: Vec<u16>,
    pub ttl_seconds: u64,
    pub renew_before_seconds: u64,
    pub invalidation_statuses: Vec<u16>,
    pub require_served_model_match: bool,
}

impl Default for TurnStatePolicy {
    fn default() -> Self {
        Self {
            capture_business_responses: true,
            capture_probe_responses: true,
            injection_enabled: true,
            response_header_names: vec!["x-codex-turn-state".to_owned()],
            response_json_pointers: vec![
                "/current_turn_state".to_owned(),
                "/error/current_turn_state".to_owned(),
            ],
            // 新旧上游格式并存；只接受明确配置的长度。
            accepted_lengths: vec![292, 332],
            ttl_seconds: 60 * 60,
            renew_before_seconds: 5 * 60,
            invalidation_statuses: vec![312],
            require_served_model_match: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseHeaderCarrySource {
    BusinessResponse,
    TurnStateProbe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseHeaderValueSelection {
    First,
    Last,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseHeaderMergeMode {
    IfAbsent,
    Replace,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseHeaderCarryScope {
    Account,
    AccountModel,
    AccountModelProxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseHeaderMissingBehavior {
    Keep,
    Clear,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseHeaderTransform {
    #[default]
    Direct,
    SetCookieToCookie,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponseHeaderCarryRule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub capture_enabled: bool,
    pub injection_enabled: bool,
    pub clear_on_disable: bool,
    pub sources: Vec<ResponseHeaderCarrySource>,
    pub source_header: String,
    pub target_header: String,
    #[serde(default)]
    pub transform: ResponseHeaderTransform,
    pub value_selection: ResponseHeaderValueSelection,
    pub merge_mode: ResponseHeaderMergeMode,
    pub scope: ResponseHeaderCarryScope,
    /// 空列表表示允许所有账号；非空时只匹配列出的 Provider 账号 ID。
    #[serde(default)]
    pub account_ids: Vec<String>,
    /// 空列表表示允许所有模型；非空时按 Provider 规范化后的模型 ID 精确匹配。
    #[serde(default)]
    pub models: Vec<String>,
    pub ttl_seconds: u64,
    pub missing_behavior: ResponseHeaderMissingBehavior,
    pub capture_status_min: u16,
    pub capture_status_max: u16,
    pub invalidation_statuses: Vec<u16>,
    pub max_value_bytes: u32,
    pub max_values: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ResponseHeaderCarryPolicy {
    pub rules: Vec<ResponseHeaderCarryRule>,
}

/// 清理运行态时所有筛选条件都按交集解释；空条件表示全部。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnStateRuntimeClear {
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub rule_id: Option<String>,
    #[serde(default = "default_true")]
    pub response_headers: bool,
    #[serde(default)]
    pub turn_state: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseHeaderCarryStatus {
    pub total_entries: usize,
    pub rules: Vec<ResponseHeaderCarryRuleStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseHeaderCarryRuleStatus {
    pub rule_id: String,
    pub cached_entries: usize,
    pub last_updated_at: Option<DateTime<Utc>>,
}

impl TurnStateProbePolicy {
    pub fn validate(&self) -> Result<(), super::AdminError> {
        if !(1..=3).contains(&self.candidate_limit) {
            return Err(super::AdminError::invalid("每轮最多尝试数量应为 1～3"));
        }
        let unique = self
            .proxy_ids
            .iter()
            .collect::<std::collections::HashSet<_>>();
        if self.proxy_ids.len() > 200
            || unique.len() != self.proxy_ids.len()
            || self.proxy_ids.iter().any(|id| {
                id.is_empty()
                    || id.len() > 128
                    || id.chars().any(char::is_whitespace)
                    || id.chars().any(char::is_control)
            })
        {
            return Err(super::AdminError::invalid(
                "代理选择不合法，最多选择 200 个不同代理",
            ));
        }
        match self.mode {
            TurnStateProxyMode::Fixed if self.proxy_ids.len() != 1 || self.candidate_limit != 1 => {
                Err(super::AdminError::invalid(
                    "固定代理必须选择一个代理，最多尝试数量为 1",
                ))
            }
            TurnStateProxyMode::Pool if self.proxy_ids.is_empty() => {
                Err(super::AdminError::invalid("请选择代理池中的代理"))
            }
            TurnStateProxyMode::Smart if !self.proxy_ids.is_empty() => {
                Err(super::AdminError::invalid("智能选择使用全部已保存代理"))
            }
            _ => Ok(()),
        }?;
        if self.schema_version != 3 {
            return Err(super::AdminError::invalid("不支持的策略版本"));
        }
        self.verification.validate()?;
        if self.verification.mode == TurnStateVerificationMode::MintAndValidate
            && self.schedule.budget_limit < u16::from(self.verification.reuse_count) + 1
        {
            return Err(super::AdminError::invalid(
                "窗口请求上限不足以完成铸票和全部验证",
            ));
        }
        self.validate_schedule()?;
        self.validate_request(&self.request)?;
        self.validate_request(&self.reuse_request)?;
        self.validate_state()?;
        self.validate_header_carry()?;
        if serde_json::to_vec(self).map_or(true, |value| value.len() > 32_768) {
            return Err(super::AdminError::invalid("探测策略超过 32 KiB 保存上限"));
        }
        Ok(())
    }

    pub fn allows(&self, proxy_id: &str) -> bool {
        self.proxy_ids.is_empty() || self.proxy_ids.iter().any(|id| id == proxy_id)
    }

    fn validate_schedule(&self) -> Result<(), super::AdminError> {
        let value = &self.schedule;
        if !(10..=3_600).contains(&value.scan_interval_seconds)
            || !(1..=86_400).contains(&value.round_min_interval_seconds)
            || !(100..=300_000).contains(&value.request_spacing_milliseconds)
            || !(1..=300).contains(&value.request_timeout_seconds)
            || !(60..=604_800).contains(&value.activity_window_seconds)
            || !(1..=3_600).contains(&value.budget_window_seconds)
            || !(1..=100).contains(&value.budget_limit)
            || !(1..=16).contains(&value.max_concurrency)
            || !(1..=3_600).contains(&value.retry_initial_seconds)
            || value.retry_initial_seconds > value.retry_max_seconds
            || value.retry_max_seconds > 86_400
            || !(1..=3_600).contains(&value.proxy_cooldown_initial_seconds)
            || value.proxy_cooldown_initial_seconds > value.proxy_cooldown_max_seconds
            || value.proxy_cooldown_max_seconds > 86_400
        {
            return Err(super::AdminError::invalid("探测调度参数超出允许范围"));
        }
        Ok(())
    }

    fn validate_request(&self, value: &TurnStateProbeRequest) -> Result<(), super::AdminError> {
        if !(-7..=22).contains(&value.compression_level)
            || value.input_text.is_empty()
            || value.input_text.len() > 16_384
            || value
                .instructions
                .as_ref()
                .is_some_and(|item| item.len() > 16_384)
            || value.reasoning_effort.as_ref().is_some_and(|item| {
                !matches!(
                    item.as_str(),
                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
                )
            })
            || value.include.len() > 32
            || value
                .include
                .iter()
                .any(|item| item.is_empty() || item.len() > 256)
            || value
                .service_tier
                .as_ref()
                .is_some_and(|item| item.is_empty() || item.len() > 64)
            || !(1_024..=1_048_576).contains(&value.max_response_body_bytes)
            || serde_json::to_vec(&value.extra_body).map_or(true, |body| body.len() > 16_384)
            || value.extra_headers.len() > 32
        {
            return Err(super::AdminError::invalid(
                "探测请求参数不合法或超过大小限制",
            ));
        }
        if value
            .body
            .as_ref()
            .is_some_and(|body| serde_json::to_vec(body).map_or(true, |bytes| bytes.len() > 16_384))
        {
            return Err(super::AdminError::invalid("请求体模板超过大小限制"));
        }
        const OWNED_BODY_FIELDS: &[&str] = &[
            "model",
            "input",
            "instructions",
            "reasoning",
            "stream",
            "store",
            "parallel_tool_calls",
            "include",
            "service_tier",
        ];
        if value
            .extra_body
            .keys()
            .any(|key| OWNED_BODY_FIELDS.contains(&key.as_str()))
        {
            return Err(super::AdminError::invalid(
                "额外请求体不能覆盖结构化探测字段",
            ));
        }
        for header in &value.extra_headers {
            normalized_header_name(&header.name)?;
            if header.value.is_empty()
                || header.value.len() > 8_192
                || header.value.contains(['\r', '\n'])
            {
                return Err(super::AdminError::invalid(
                    "探测请求头格式不合法或超过大小限制",
                ));
            }
        }
        Ok(())
    }

    fn validate_state(&self) -> Result<(), super::AdminError> {
        let value = &self.state;
        if value.response_header_names.is_empty()
            || value.response_header_names.len() > 16
            || value.response_header_names.iter().any(|name| {
                normalized_header_name(name)
                    .map_or(true, |name| response_header_is_sensitive(&name))
            })
            || value.response_json_pointers.len() > 16
            || value
                .response_json_pointers
                .iter()
                .any(|path| !path.starts_with('/') || path.len() > 256)
            || value.accepted_lengths.is_empty()
            || value.accepted_lengths.len() > 16
            || value
                .accepted_lengths
                .iter()
                .any(|size| !(1..=8_192).contains(size))
            || value.ttl_seconds < 60
            || value.ttl_seconds > 604_800
            || value.renew_before_seconds >= value.ttl_seconds
            || value.invalidation_statuses.len() > 32
            || value
                .invalidation_statuses
                .iter()
                .any(|status| !(100..=599).contains(status))
        {
            return Err(super::AdminError::invalid("State 提取或有效期参数不合法"));
        }
        Ok(())
    }

    fn validate_header_carry(&self) -> Result<(), super::AdminError> {
        let rules = &self.response_header_carry.rules;
        if rules.len() > 64 {
            return Err(super::AdminError::invalid("响应头续带规则最多 64 条"));
        }
        let mut ids = std::collections::HashSet::new();
        for rule in rules {
            let source = normalized_header_name(&rule.source_header)?;
            let target = normalized_header_name(&rule.target_header)?;
            let cookie_mapping = rule.transform == ResponseHeaderTransform::SetCookieToCookie
                && source == "set-cookie"
                && target == "cookie";
            if rule.id.is_empty()
                || rule.id.len() > 64
                || !rule
                    .id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
                || !ids.insert(rule.id.as_str())
                || rule.name.is_empty()
                || rule.name.len() > 128
                || rule.sources.is_empty()
                || rule.sources.len() > 2
                || (rule.transform == ResponseHeaderTransform::SetCookieToCookie && !cookie_mapping)
                || rule.account_ids.len() > 200
                || rule
                    .account_ids
                    .iter()
                    .any(|value| value.is_empty() || value.len() > 128)
                || rule.models.len() > 200
                || rule
                    .models
                    .iter()
                    .any(|value| value.is_empty() || value.len() > 256)
                || !(1..=604_800).contains(&rule.ttl_seconds)
                || !(100..=599).contains(&rule.capture_status_min)
                || rule.capture_status_min > rule.capture_status_max
                || rule.capture_status_max > 599
                || rule.invalidation_statuses.len() > 32
                || rule
                    .invalidation_statuses
                    .iter()
                    .any(|status| !(100..=599).contains(status))
                || !(1..=65_536).contains(&rule.max_value_bytes)
                || !(1..=16).contains(&rule.max_values)
            {
                return Err(super::AdminError::invalid(
                    "响应头续带规则不合法或包含保留字段",
                ));
            }
        }
        Ok(())
    }
}

fn normalized_header_name(value: &str) -> Result<String, super::AdminError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
    {
        return Err(super::AdminError::invalid("HTTP 请求头名称不合法"));
    }
    Ok(value)
}

fn response_header_is_sensitive(name: &str) -> bool {
    name.starts_with("proxy-")
        || matches!(
            name,
            "authorization"
                | "x-api-key"
                | "www-authenticate"
                | "authentication-info"
                | "cookie"
                | "cookie2"
                | "set-cookie"
                | "set-cookie2"
                | "chatgpt-account-id"
                | "chatgpt-organization-id"
                | "chatgpt-org-id"
                | "chatgpt-project-id"
                | "openai-organization"
                | "openai-project"
                | "x-openai-organization"
                | "x-openai-project"
                | "x-codex-installation-id"
                | "x-codex-turn-metadata"
                | "x-oai-attestation"
                | "x-oai-is"
                | "x-oai-is-update"
        )
}

/// 两阶段验证的规则只描述可观测事实，不执行用户脚本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStateVerificationMode {
    AcquireOnly,
    MintAndValidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStateBusinessProxy {
    FollowVerified,
    MatchAccount,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct TurnStateVerification {
    pub mode: TurnStateVerificationMode,
    pub reuse_count: u8,
    pub mint_to_reuse_delay_milliseconds: u64,
    pub reuse_spacing_milliseconds: u64,
    pub round_timeout_seconds: u64,
    pub stop_on_first_failure: bool,
    pub required_cookie_names: Vec<String>,
    pub business_proxy: TurnStateBusinessProxy,
    pub mint_success: TurnStateSuccessRules,
    pub reuse_success: TurnStateSuccessRules,
}
impl Default for TurnStateVerification {
    fn default() -> Self {
        Self {
            mode: TurnStateVerificationMode::MintAndValidate,
            reuse_count: 3,
            mint_to_reuse_delay_milliseconds: 100,
            reuse_spacing_milliseconds: 100,
            round_timeout_seconds: 300,
            stop_on_first_failure: true,
            required_cookie_names: vec!["__cflb".to_owned(), "__oailb".to_owned()],
            business_proxy: TurnStateBusinessProxy::FollowVerified,
            mint_success: TurnStateSuccessRules::default(),
            reuse_success: TurnStateSuccessRules::default(),
        }
    }
}
impl TurnStateVerification {
    fn validate(&self) -> Result<(), super::AdminError> {
        if !(1..=10).contains(&self.reuse_count)
            || self.mint_to_reuse_delay_milliseconds > 300_000
            || self.reuse_spacing_milliseconds > 300_000
            || !(1..=3600).contains(&self.round_timeout_seconds)
            || self.required_cookie_names.len() > 16
            || self
                .required_cookie_names
                .iter()
                .any(|name| normalized_header_name(name).is_err())
        {
            return Err(super::AdminError::invalid("复用验证参数不合法"));
        }
        for rule in [&self.mint_success, &self.reuse_success] {
            if rule.status_min < 200
                || rule.status_max > 299
                || rule.status_min > rule.status_max
                || rule.expected_models.len() > 32
                || rule
                    .expected_models
                    .iter()
                    .any(|v| v.is_empty() || v.len() > 256)
                || rule.json_predicates.len() > 16
                || rule.json_predicates.iter().any(|p| {
                    !p.pointer.starts_with('/')
                        || p.pointer.len() > 256
                        || p.values.is_empty()
                        || p.values.len() > 32
                })
            {
                return Err(super::AdminError::invalid("验证成功条件不合法"));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct TurnStateSuccessRules {
    pub status_min: u16,
    pub status_max: u16,
    pub require_completed: bool,
    pub require_model_match: bool,
    /// 空列表匹配本轮选定模型，非空列表精确匹配其中任意值。
    pub expected_models: Vec<String>,
    pub json_predicates: Vec<TurnStateJsonPredicate>,
}
impl Default for TurnStateSuccessRules {
    fn default() -> Self {
        Self {
            status_min: 200,
            status_max: 299,
            require_completed: true,
            require_model_match: true,
            expected_models: Vec::new(),
            json_predicates: Vec::new(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnStateJsonPredicate {
    pub pointer: String,
    pub values: Vec<serde_json::Value>,
}

fn default_mint_body() -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({
        "model": "$model", "instructions": "Reply with OK. Do not call tools.",
        "input": [
            {"type": "additional_tools", "role": "developer", "tools": [
                {"type": "namespace", "name": "codex", "description": "local tools", "tools": [
                    {"type": "function", "name": "noop", "description": "Do nothing.", "strict": false,
                     "parameters": {"type": "object", "properties": {}, "additionalProperties": false}}
                ]}
            ]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Reply with OK. Do not call tools."}]}
        ],
        "stream": true, "store": false, "parallel_tool_calls": false,
        "include": ["reasoning.encrypted_content"], "reasoning": {"context": "all_turns"}
    }).as_object().cloned().unwrap_or_default()
}

impl TurnStateProbeRequest {
    pub fn reuse_default() -> Self {
        let mut request = Self::default();
        if let Some(body) = &mut request.body {
            body.insert(
                "instructions".to_owned(),
                serde_json::json!("Reply to the user. Do not call tools."),
            );
        }
        request
    }
}

fn replace_model(value: &mut serde_json::Value, model: &str) {
    match value {
        serde_json::Value::String(text) if text == "$model" => *text = model.to_owned(),
        serde_json::Value::Array(values) => values.iter_mut().for_each(|v| replace_model(v, model)),
        serde_json::Value::Object(values) => {
            values.values_mut().for_each(|v| replace_model(v, model))
        }
        _ => {}
    }
}

impl TurnStateProbeRequest {
    pub fn render_body(&self, model: &str) -> serde_json::Value {
        let request = self;
        let mut body = request.body.clone().unwrap_or_else(|| {
        let mut body = request.extra_body.clone();
        body.insert("model".into(), serde_json::Value::String(model.to_owned()));
        body.insert("input".into(), serde_json::json!([{"type":"message", "role":"user", "content":[{"type":"input_text", "text":request.input_text}]}]));
        body.insert("stream".into(), serde_json::Value::Bool(request.stream));
        body.insert("store".into(), serde_json::Value::Bool(request.store));
        if let Some(value) = &request.instructions { body.insert("instructions".into(), value.clone().into()); }
        if let Some(value) = &request.reasoning_effort { body.insert("reasoning".into(), serde_json::json!({"effort":value})); }
        if let Some(value) = request.parallel_tool_calls { body.insert("parallel_tool_calls".into(), value.into()); }
        if !request.include.is_empty() { body.insert("include".into(), serde_json::json!(request.include)); }
        if let Some(value) = &request.service_tier { body.insert("service_tier".into(), value.clone().into()); }
        body
    });
        for value in body.values_mut() {
            replace_model(value, model);
        }
        serde_json::Value::Object(body)
    }
}
