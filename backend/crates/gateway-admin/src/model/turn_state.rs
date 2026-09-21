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
