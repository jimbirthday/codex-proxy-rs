//! Codex turn state 探测报头的非阻塞写入与管理查询端口。

use std::time::Duration;

use async_trait::async_trait;

use crate::model::turn_state_capture::{
    TurnStateCaptureStatus, TurnStateProbeExchangeCapture, TurnStateProbeExchangeDetail,
    TurnStateProbeExchangePage, TurnStateProbeExchangeQuery,
};

use super::store::AdminStoreResult;
use super::store::{AdminStoreError, AdminStoreErrorKind};

/// Provider 只能检查采集窗口并提交可丢失观测，不能查询数据库或修改采集策略。
pub trait TurnStateProbeCaptureSink: Send + Sync {
    fn enabled(&self) -> bool;
    /// 成功进入有界队列时返回 `true`；调用方不能等待或重试。
    fn try_capture(&self, capture: TurnStateProbeExchangeCapture) -> bool;
}

/// Admin 拥有采集窗口、状态和持久记录查询。
#[async_trait]
pub trait TurnStateProbeCaptureStore: Send + Sync {
    fn status(&self) -> TurnStateCaptureStatus;
    fn start(&self, duration: Duration) -> TurnStateCaptureStatus;
    fn stop(&self) -> TurnStateCaptureStatus;

    async fn list(
        &self,
        query: TurnStateProbeExchangeQuery,
    ) -> AdminStoreResult<TurnStateProbeExchangePage>;

    async fn detail(&self, id: &str) -> AdminStoreResult<Option<TurnStateProbeExchangeDetail>>;
}

/// 未接入 Store 的 Provider 初始化与单元测试使用，保持探测行为不变。
#[derive(Debug, Default)]
pub struct DisabledTurnStateProbeCapture;

impl TurnStateProbeCaptureSink for DisabledTurnStateProbeCapture {
    fn enabled(&self) -> bool {
        false
    }

    fn try_capture(&self, _capture: TurnStateProbeExchangeCapture) -> bool {
        false
    }
}

#[async_trait]
impl TurnStateProbeCaptureStore for DisabledTurnStateProbeCapture {
    fn status(&self) -> TurnStateCaptureStatus {
        TurnStateCaptureStatus {
            enabled_until: None,
            retention_hours: 24,
            buffer: crate::model::turn_state_capture::TurnStateCaptureBufferStats {
                queued_items: 0,
                queued_bytes: 0,
                enqueued_total: 0,
                dropped_total: 0,
                persisted_total: 0,
                write_failure_total: 0,
            },
        }
    }

    fn start(&self, _duration: Duration) -> TurnStateCaptureStatus {
        self.status()
    }

    fn stop(&self) -> TurnStateCaptureStatus {
        self.status()
    }

    async fn list(
        &self,
        _query: TurnStateProbeExchangeQuery,
    ) -> AdminStoreResult<TurnStateProbeExchangePage> {
        Err(disabled_store_error())
    }

    async fn detail(&self, _id: &str) -> AdminStoreResult<Option<TurnStateProbeExchangeDetail>> {
        Err(disabled_store_error())
    }
}

fn disabled_store_error() -> AdminStoreError {
    AdminStoreError::new(
        AdminStoreErrorKind::Unavailable,
        "turn state probe capture",
        "capture store is disabled",
    )
}
