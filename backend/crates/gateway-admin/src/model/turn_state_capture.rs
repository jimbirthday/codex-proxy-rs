//! Codex turn state 探测报头的短期敏感观测模型。

use std::fmt;

use chrono::{DateTime, Utc};

use super::{PageSize, turn_state::TurnStateSource};

/// 单个 HTTP Header 值；重复名称保留为多个条目，顺序只表示 HeaderMap 的可见顺序。
#[derive(Clone, PartialEq, Eq)]
pub struct TurnStateProbeHeader {
    pub name: String,
    pub value: Vec<u8>,
}

impl fmt::Debug for TurnStateProbeHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnStateProbeHeader")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .field("value_bytes", &self.value.len())
            .finish()
    }
}

/// Provider 在一次实际探测请求结束后提交的完整报头事实。
#[derive(Clone, PartialEq, Eq)]
pub struct TurnStateProbeExchangeCapture {
    pub id: String,
    pub trigger: TurnStateSource,
    pub account_id: String,
    pub model: String,
    pub target_id: String,
    pub target_label: String,
    pub request_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status_code: Option<u16>,
    pub http_version: Option<String>,
    pub outcome: String,
    pub latency_ms: u64,
    pub request_headers: Vec<TurnStateProbeHeader>,
    pub response_headers: Vec<TurnStateProbeHeader>,
}

impl TurnStateProbeExchangeCapture {
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let header_bytes = self
            .request_headers
            .iter()
            .chain(&self.response_headers)
            .fold(0_usize, |total, header| {
                total
                    .saturating_add(header.name.len())
                    .saturating_add(header.value.len())
            });
        header_bytes
            .saturating_add(self.id.len())
            .saturating_add(self.account_id.len())
            .saturating_add(self.model.len())
            .saturating_add(self.target_id.len())
            .saturating_add(self.target_label.len())
            .saturating_add(self.request_id.len())
            .saturating_add(self.outcome.len())
    }
}

impl fmt::Debug for TurnStateProbeExchangeCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnStateProbeExchangeCapture")
            .field("id", &self.id)
            .field("trigger", &self.trigger)
            .field("account_id", &self.account_id)
            .field("model", &self.model)
            .field("target_id", &self.target_id)
            .field("request_id", &self.request_id)
            .field("status_code", &self.status_code)
            .field("request_header_count", &self.request_headers.len())
            .field("response_header_count", &self.response_headers.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnStateCaptureBufferStats {
    pub queued_items: usize,
    pub queued_bytes: usize,
    pub enqueued_total: u64,
    pub dropped_total: u64,
    pub persisted_total: u64,
    pub write_failure_total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnStateCaptureStatus {
    pub enabled_until: Option<DateTime<Utc>>,
    pub retention_hours: u32,
    pub buffer: TurnStateCaptureBufferStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateProbeExchangeQuery {
    pub page: u32,
    pub page_size: PageSize,
    pub account_id: Option<String>,
    pub model: Option<String>,
    pub trigger: Option<TurnStateSource>,
    pub status_code: Option<u16>,
    pub state_only: bool,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateHeaderSummary {
    pub count: u32,
    pub present: bool,
    pub byte_length: Option<u32>,
    pub sha256: Option<String>,
    pub valid_292: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateProbeExchangeSummary {
    pub id: String,
    pub trigger: TurnStateSource,
    pub account_id: String,
    pub model: String,
    pub target_id: String,
    pub target_label: String,
    pub request_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status_code: Option<u16>,
    pub http_version: Option<String>,
    pub outcome: String,
    pub latency_ms: u64,
    pub request_header_count: u32,
    pub response_header_count: u32,
    pub request_header_bytes: u64,
    pub response_header_bytes: u64,
    pub request_turn_state: TurnStateHeaderSummary,
    pub response_turn_state: TurnStateHeaderSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateProbeExchangePage {
    pub items: Vec<TurnStateProbeExchangeSummary>,
    pub page: u32,
    pub page_size: u16,
    pub total: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct TurnStateProbeExchangeDetail {
    pub summary: TurnStateProbeExchangeSummary,
    pub request_headers: Vec<TurnStateProbeHeader>,
    pub response_headers: Vec<TurnStateProbeHeader>,
}

impl fmt::Debug for TurnStateProbeExchangeDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnStateProbeExchangeDetail")
            .field("summary", &self.summary)
            .field("request_header_count", &self.request_headers.len())
            .field("response_header_count", &self.response_headers.len())
            .finish()
    }
}

#[must_use]
pub fn is_sensitive_probe_header(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization"
            | "cookie"
            | "set-cookie"
            | "proxy-authorization"
            | "x-api-key"
            | "api-key"
            | "x-codex-turn-state"
    ) || name.contains("authorization")
        || name.contains("cookie")
        || name.contains("token")
        || name.contains("secret")
        || name.contains("credential")
        || name.contains("api-key")
        || name.contains("apikey")
}
