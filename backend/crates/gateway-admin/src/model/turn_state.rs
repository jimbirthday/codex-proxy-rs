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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnStateProbePolicy {
    pub manual_enabled: bool,
    pub automatic_enabled: bool,
    pub mode: TurnStateProxyMode,
    /// 随机模式为空时使用完整目录；固定及代理池模式必须显式选择。
    pub proxy_ids: Vec<String>,
    pub candidate_limit: u8,
}

impl Default for TurnStateProbePolicy {
    fn default() -> Self {
        Self {
            manual_enabled: true,
            automatic_enabled: true,
            mode: TurnStateProxyMode::Smart,
            proxy_ids: Vec::new(),
            candidate_limit: 3,
        }
    }
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
        }
    }

    pub fn allows(&self, proxy_id: &str) -> bool {
        self.proxy_ids.is_empty() || self.proxy_ids.iter().any(|id| id == proxy_id)
    }
}
