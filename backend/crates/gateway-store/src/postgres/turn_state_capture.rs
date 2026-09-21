//! Codex turn state 探测报头的短期存储、有界队列与管理查询。

use std::{
    num::NonZeroUsize,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use gateway_admin::{
    model::turn_state::TurnStateSource,
    model::turn_state_capture::{
        TurnStateCaptureBufferStats, TurnStateCaptureStatus, TurnStateHeaderSummary,
        TurnStateProbeExchangeCapture, TurnStateProbeExchangeDetail, TurnStateProbeExchangePage,
        TurnStateProbeExchangeQuery, TurnStateProbeExchangeSummary, TurnStateProbeHeader,
    },
    ports::{
        store::{AdminStoreError, AdminStoreResult},
        turn_state_capture::{TurnStateProbeCaptureSink, TurnStateProbeCaptureStore},
    },
};
use gateway_core::{
    lifecycle::CancellationToken,
    task::{DaemonTask, WorkerTaskError},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, postgres::PgRow};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::postgres_unavailable;

const QUEUE_CAPACITY: usize = 256;
const QUEUE_BYTE_CAPACITY: usize = 8 * 1024 * 1024;
const BATCH_SIZE: usize = 32;
const BATCH_WAIT: Duration = Duration::from_millis(100);
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const RETENTION_HOURS: u32 = 24;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredHeader {
    name: String,
    value_base64: String,
}

struct CaptureRuntimeState {
    enabled_until: Mutex<Option<SystemTime>>,
    maximum_queued_bytes: usize,
    queued_items: AtomicUsize,
    queued_bytes: AtomicUsize,
    enqueued_total: AtomicU64,
    dropped_total: AtomicU64,
    persisted_total: AtomicU64,
    write_failure_total: AtomicU64,
}

impl CaptureRuntimeState {
    fn new() -> Self {
        Self {
            enabled_until: Mutex::new(None),
            maximum_queued_bytes: QUEUE_BYTE_CAPACITY,
            queued_items: AtomicUsize::new(0),
            queued_bytes: AtomicUsize::new(0),
            enqueued_total: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
            persisted_total: AtomicU64::new(0),
            write_failure_total: AtomicU64::new(0),
        }
    }

    fn enabled_until(&self) -> Option<SystemTime> {
        let mut enabled_until = self
            .enabled_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if enabled_until.is_some_and(|until| until <= SystemTime::now()) {
            *enabled_until = None;
        }
        *enabled_until
    }

    fn reserve(&self, bytes: usize) -> bool {
        let reserved = self
            .queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.maximum_queued_bytes)
            })
            .is_ok();
        if reserved {
            self.queued_items.fetch_add(1, Ordering::Relaxed);
        }
        reserved
    }

    fn release(&self, bytes: usize) {
        self.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.queued_items.fetch_sub(1, Ordering::AcqRel);
    }

    fn stats(&self) -> TurnStateCaptureBufferStats {
        TurnStateCaptureBufferStats {
            queued_items: self.queued_items.load(Ordering::Acquire),
            queued_bytes: self.queued_bytes.load(Ordering::Acquire),
            enqueued_total: self.enqueued_total.load(Ordering::Acquire),
            dropped_total: self.dropped_total.load(Ordering::Acquire),
            persisted_total: self.persisted_total.load(Ordering::Acquire),
            write_failure_total: self.write_failure_total.load(Ordering::Acquire),
        }
    }
}

struct QueuedCapture {
    capture: Option<TurnStateProbeExchangeCapture>,
    estimated_bytes: usize,
    state: Arc<CaptureRuntimeState>,
}

impl Drop for QueuedCapture {
    fn drop(&mut self) {
        self.state.release(self.estimated_bytes);
    }
}

#[derive(Clone)]
pub struct PgTurnStateProbeCapture {
    repository: Arc<PgTurnStateProbeCaptureRepository>,
    sender: mpsc::Sender<QueuedCapture>,
    state: Arc<CaptureRuntimeState>,
}

impl PgTurnStateProbeCapture {
    #[must_use]
    pub fn new(pool: PgPool) -> (Self, TurnStateProbeCaptureWriter) {
        let repository = Arc::new(PgTurnStateProbeCaptureRepository { pool });
        let (sender, receiver) = mpsc::channel(
            NonZeroUsize::new(QUEUE_CAPACITY)
                .expect("turn state capture queue capacity is non-zero")
                .get(),
        );
        let state = Arc::new(CaptureRuntimeState::new());
        (
            Self {
                repository: Arc::clone(&repository),
                sender,
                state: Arc::clone(&state),
            },
            TurnStateProbeCaptureWriter {
                repository,
                receiver: AsyncMutex::new(receiver),
                state,
            },
        )
    }

    fn capture_status(&self) -> TurnStateCaptureStatus {
        TurnStateCaptureStatus {
            enabled_until: self.state.enabled_until().map(DateTime::<Utc>::from),
            retention_hours: RETENTION_HOURS,
            buffer: self.state.stats(),
        }
    }
}

impl TurnStateProbeCaptureSink for PgTurnStateProbeCapture {
    fn enabled(&self) -> bool {
        self.state.enabled_until().is_some()
    }

    fn try_capture(&self, capture: TurnStateProbeExchangeCapture) -> bool {
        let estimated_bytes = capture.estimated_bytes().max(1);
        if !self.state.reserve(estimated_bytes) {
            saturating_increment(&self.state.dropped_total, 1);
            tracing::warn!(
                exchange_id = %capture.id,
                estimated_bytes,
                maximum_queued_bytes = self.state.maximum_queued_bytes,
                "Turn state 探测报头队列字节预算不足，已丢弃观测"
            );
            return false;
        }
        let queued = QueuedCapture {
            capture: Some(capture),
            estimated_bytes,
            state: Arc::clone(&self.state),
        };
        match self.sender.try_send(queued) {
            Ok(()) => {
                saturating_increment(&self.state.enqueued_total, 1);
                true
            }
            Err(error) => {
                let queued = match error {
                    mpsc::error::TrySendError::Full(queued)
                    | mpsc::error::TrySendError::Closed(queued) => queued,
                };
                let exchange_id = queued.capture.as_ref().map(|capture| capture.id.clone());
                drop(queued);
                saturating_increment(&self.state.dropped_total, 1);
                tracing::warn!(
                    exchange_id = ?exchange_id,
                    "Turn state 探测报头队列不可用，已丢弃观测"
                );
                false
            }
        }
    }
}

#[async_trait]
impl TurnStateProbeCaptureStore for PgTurnStateProbeCapture {
    fn status(&self) -> TurnStateCaptureStatus {
        self.capture_status()
    }

    fn start(&self, duration: Duration) -> TurnStateCaptureStatus {
        let until = SystemTime::now() + duration;
        *self
            .state
            .enabled_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(until);
        self.capture_status()
    }

    fn stop(&self) -> TurnStateCaptureStatus {
        *self
            .state
            .enabled_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.capture_status()
    }

    async fn list(
        &self,
        query: TurnStateProbeExchangeQuery,
    ) -> AdminStoreResult<TurnStateProbeExchangePage> {
        self.repository.list(query).await
    }

    async fn detail(&self, id: &str) -> AdminStoreResult<Option<TurnStateProbeExchangeDetail>> {
        self.repository.detail(id).await
    }
}

pub struct TurnStateProbeCaptureWriter {
    repository: Arc<PgTurnStateProbeCaptureRepository>,
    receiver: AsyncMutex<mpsc::Receiver<QueuedCapture>>,
    state: Arc<CaptureRuntimeState>,
}

impl DaemonTask for TurnStateProbeCaptureWriter {
    fn run(
        &self,
        cancellation: CancellationToken,
    ) -> futures::future::BoxFuture<'_, Result<(), WorkerTaskError>> {
        Box::pin(async move {
            let mut receiver = self.receiver.lock().await;
            loop {
                let first = tokio::select! {
                    () = cancellation.cancelled() => {
                        drain_on_shutdown(&mut receiver, &self.repository, &self.state).await;
                        return Ok(());
                    }
                    item = receiver.recv() => item,
                };
                let Some(first) = first else {
                    return Err(WorkerTaskError::safe("turn state capture queue closed"));
                };
                let mut batch = vec![first];
                let deadline = tokio::time::Instant::now() + BATCH_WAIT;
                while batch.len() < BATCH_SIZE {
                    match tokio::time::timeout_at(deadline, receiver.recv()).await {
                        Ok(Some(item)) => batch.push(item),
                        Ok(None) | Err(_) => break,
                    }
                }
                persist_batch(batch, &self.repository, &self.state).await;
            }
        })
    }
}

async fn persist_batch(
    mut queued: Vec<QueuedCapture>,
    repository: &PgTurnStateProbeCaptureRepository,
    state: &CaptureRuntimeState,
) {
    let captures = queued
        .iter_mut()
        .filter_map(|queued| queued.capture.take())
        .collect::<Vec<_>>();
    if captures.is_empty() {
        return;
    }
    match repository.insert_batch(&captures).await {
        Ok(()) => saturating_increment(
            &state.persisted_total,
            u64::try_from(captures.len()).unwrap_or(u64::MAX),
        ),
        Err(error) => {
            saturating_increment(
                &state.write_failure_total,
                u64::try_from(captures.len()).unwrap_or(u64::MAX),
            );
            tracing::warn!(
                batch_size = captures.len(),
                error = %error,
                "Turn state 探测报头批量写入失败"
            );
        }
    }
}

async fn drain_on_shutdown(
    receiver: &mut mpsc::Receiver<QueuedCapture>,
    repository: &PgTurnStateProbeCaptureRepository,
    state: &CaptureRuntimeState,
) {
    receiver.close();
    let started_at = Instant::now();
    while started_at.elapsed() < SHUTDOWN_DRAIN_TIMEOUT {
        let mut batch = Vec::with_capacity(BATCH_SIZE);
        while batch.len() < BATCH_SIZE {
            match receiver.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
        }
        if batch.is_empty() {
            break;
        }
        let remaining = SHUTDOWN_DRAIN_TIMEOUT.saturating_sub(started_at.elapsed());
        if tokio::time::timeout(remaining, persist_batch(batch, repository, state))
            .await
            .is_err()
        {
            break;
        }
    }
    let mut dropped = 0_u64;
    while let Ok(item) = receiver.try_recv() {
        drop(item);
        dropped = dropped.saturating_add(1);
    }
    saturating_increment(&state.dropped_total, dropped);
}

struct PgTurnStateProbeCaptureRepository {
    pool: PgPool,
}

impl PgTurnStateProbeCaptureRepository {
    async fn insert_batch(
        &self,
        captures: &[TurnStateProbeExchangeCapture],
    ) -> crate::StoreResult<()> {
        if captures.is_empty() {
            return Ok(());
        }
        let mut builder = QueryBuilder::<Postgres>::new(
            "insert into turn_state_probe_exchanges (id, trigger, account_id, model_id, target_id, target_label, request_id, started_at, finished_at, status_code, http_version, outcome, latency_ms, request_headers_json, response_headers_json, request_header_count, response_header_count, request_header_bytes, response_header_bytes, request_turn_state_count, request_turn_state_length, request_turn_state_sha256, response_turn_state_count, response_turn_state_length, response_turn_state_sha256, expires_at) ",
        );
        builder.push_values(captures, |mut row, capture| {
            let request_headers = stored_headers(&capture.request_headers);
            let response_headers = stored_headers(&capture.response_headers);
            let request_bytes = header_bytes(&capture.request_headers);
            let response_bytes = header_bytes(&capture.response_headers);
            let request_state = turn_state_summary(&capture.request_headers);
            let response_state = turn_state_summary(&capture.response_headers);
            row.push_bind(&capture.id)
                .push_bind(capture.trigger.as_str())
                .push_bind(&capture.account_id)
                .push_bind(&capture.model)
                .push_bind(&capture.target_id)
                .push_bind(&capture.target_label)
                .push_bind(&capture.request_id)
                .push_bind(capture.started_at)
                .push_bind(capture.finished_at)
                .push_bind(capture.status_code.map(i32::from))
                .push_bind(&capture.http_version)
                .push_bind(&capture.outcome)
                .push_bind(i64::try_from(capture.latency_ms).unwrap_or(i64::MAX))
                .push_bind(sqlx::types::Json(request_headers))
                .push_bind(sqlx::types::Json(response_headers))
                .push_bind(i32::try_from(capture.request_headers.len()).unwrap_or(i32::MAX))
                .push_bind(i32::try_from(capture.response_headers.len()).unwrap_or(i32::MAX))
                .push_bind(i64::try_from(request_bytes).unwrap_or(i64::MAX))
                .push_bind(i64::try_from(response_bytes).unwrap_or(i64::MAX))
                .push_bind(request_state.0)
                .push_bind(request_state.1.as_ref().map(|(length, _)| *length))
                .push_bind(request_state.1.as_ref().map(|(_, hash)| hash))
                .push_bind(response_state.0)
                .push_bind(response_state.1.as_ref().map(|(length, _)| *length))
                .push_bind(response_state.1.as_ref().map(|(_, hash)| hash))
                .push_bind(
                    capture.finished_at + chrono::Duration::hours(i64::from(RETENTION_HOURS)),
                );
        });
        builder.push(" on conflict (id) do nothing");
        builder
            .build()
            .execute(&self.pool)
            .await
            .map_err(|_| postgres_unavailable("insert turn state probe exchanges"))?;
        Ok(())
    }

    async fn list(
        &self,
        query: TurnStateProbeExchangeQuery,
    ) -> AdminStoreResult<TurnStateProbeExchangePage> {
        let trigger = query.trigger.map(TurnStateSource::as_str);
        let status = query.status_code.map(i32::from);
        let offset = i64::from(query.page.saturating_sub(1))
            .saturating_mul(i64::from(query.page_size.get()));
        let total = sqlx::query_scalar::<_, i64>(
            "select count(*) from turn_state_probe_exchanges
             where ($1::text is null or account_id = $1)
               and ($2::text is null or model_id = $2)
               and ($3::text is null or trigger = $3)
               and ($4::integer is null or status_code = $4)
               and (not $5 or request_turn_state_count > 0 or response_turn_state_count > 0)
               and ($6::timestamptz is null or started_at >= $6)
               and ($7::timestamptz is null or started_at < $7)
               and expires_at > now()",
        )
        .bind(query.account_id.as_deref())
        .bind(query.model.as_deref())
        .bind(trigger)
        .bind(status)
        .bind(query.state_only)
        .bind(query.start)
        .bind(query.end)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| admin_unavailable("list turn state probe exchanges"))?;
        let rows = sqlx::query(
            "select id, trigger, account_id, model_id, target_id, target_label, request_id,
                    started_at, finished_at, status_code, http_version, outcome, latency_ms,
                    request_header_count, response_header_count, request_header_bytes,
                    response_header_bytes, request_turn_state_count, request_turn_state_length,
                    request_turn_state_sha256, response_turn_state_count,
                    response_turn_state_length, response_turn_state_sha256
             from turn_state_probe_exchanges
             where ($1::text is null or account_id = $1)
               and ($2::text is null or model_id = $2)
               and ($3::text is null or trigger = $3)
               and ($4::integer is null or status_code = $4)
               and (not $5 or request_turn_state_count > 0 or response_turn_state_count > 0)
               and ($6::timestamptz is null or started_at >= $6)
               and ($7::timestamptz is null or started_at < $7)
               and expires_at > now()
             order by created_at desc, id desc limit $8 offset $9",
        )
        .bind(query.account_id.as_deref())
        .bind(query.model.as_deref())
        .bind(trigger)
        .bind(status)
        .bind(query.state_only)
        .bind(query.start)
        .bind(query.end)
        .bind(i64::from(query.page_size.get()))
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| admin_unavailable("list turn state probe exchanges"))?;
        let items = rows
            .iter()
            .map(summary_from_row)
            .collect::<AdminStoreResult<Vec<_>>>()?;
        Ok(TurnStateProbeExchangePage {
            items,
            page: query.page,
            page_size: query.page_size.get(),
            total: u64::try_from(total).unwrap_or_default(),
        })
    }

    async fn detail(&self, id: &str) -> AdminStoreResult<Option<TurnStateProbeExchangeDetail>> {
        let row = sqlx::query(
            "select id, trigger, account_id, model_id, target_id, target_label, request_id,
                    started_at, finished_at, status_code, http_version, outcome, latency_ms,
                    request_header_count, response_header_count, request_header_bytes,
                    response_header_bytes, request_turn_state_count, request_turn_state_length,
                    request_turn_state_sha256, response_turn_state_count,
                    response_turn_state_length, response_turn_state_sha256,
                    request_headers_json, response_headers_json
             from turn_state_probe_exchanges where id = $1 and expires_at > now()",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| admin_unavailable("read turn state probe exchange"))?;
        row.map(|row| {
            let summary = summary_from_row(&row)?;
            let request = row
                .try_get::<sqlx::types::Json<Vec<StoredHeader>>, _>("request_headers_json")
                .map_err(|_| admin_invalid("request_headers_json"))?;
            let response = row
                .try_get::<sqlx::types::Json<Vec<StoredHeader>>, _>("response_headers_json")
                .map_err(|_| admin_invalid("response_headers_json"))?;
            Ok(TurnStateProbeExchangeDetail {
                summary,
                request_headers: decoded_headers(request.0)?,
                response_headers: decoded_headers(response.0)?,
            })
        })
        .transpose()
    }
}

fn stored_headers(headers: &[TurnStateProbeHeader]) -> Vec<StoredHeader> {
    headers
        .iter()
        .map(|header| StoredHeader {
            name: header.name.clone(),
            value_base64: STANDARD.encode(&header.value),
        })
        .collect()
}

fn decoded_headers(headers: Vec<StoredHeader>) -> AdminStoreResult<Vec<TurnStateProbeHeader>> {
    headers
        .into_iter()
        .map(|header| {
            STANDARD
                .decode(header.value_base64)
                .map(|value| TurnStateProbeHeader {
                    name: header.name,
                    value,
                })
                .map_err(|_| admin_invalid("stored turn state probe header"))
        })
        .collect()
}

fn header_bytes(headers: &[TurnStateProbeHeader]) -> usize {
    headers.iter().fold(0, |total, header| {
        total
            .saturating_add(header.name.len())
            .saturating_add(header.value.len())
    })
}

fn turn_state_summary(headers: &[TurnStateProbeHeader]) -> (i32, Option<(i32, String)>) {
    let values = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("x-codex-turn-state"))
        .collect::<Vec<_>>();
    let count = i32::try_from(values.len()).unwrap_or(i32::MAX);
    if values.len() != 1 {
        return (count, None);
    }
    let value = values[0];
    (
        count,
        Some((
            i32::try_from(value.value.len()).unwrap_or(i32::MAX),
            hex::encode(Sha256::digest(&value.value)),
        )),
    )
}

fn summary_from_row(row: &PgRow) -> AdminStoreResult<TurnStateProbeExchangeSummary> {
    let trigger = match row
        .try_get::<String, _>("trigger")
        .map_err(|_| admin_invalid("trigger"))?
        .as_str()
    {
        "manual_probe" => TurnStateSource::ManualProbe,
        "automatic_renewal" => TurnStateSource::AutomaticRenewal,
        _ => return Err(admin_invalid("trigger")),
    };
    let request_length = row
        .try_get::<Option<i32>, _>("request_turn_state_length")
        .map_err(|_| admin_invalid("request_turn_state_length"))?;
    let response_length = row
        .try_get::<Option<i32>, _>("response_turn_state_length")
        .map_err(|_| admin_invalid("response_turn_state_length"))?;
    let request_count = nonnegative_u32(row, "request_turn_state_count")?;
    let response_count = nonnegative_u32(row, "response_turn_state_count")?;
    Ok(TurnStateProbeExchangeSummary {
        id: row.try_get("id").map_err(|_| admin_invalid("id"))?,
        trigger,
        account_id: row
            .try_get("account_id")
            .map_err(|_| admin_invalid("account_id"))?,
        model: row
            .try_get("model_id")
            .map_err(|_| admin_invalid("model_id"))?,
        target_id: row
            .try_get("target_id")
            .map_err(|_| admin_invalid("target_id"))?,
        target_label: row
            .try_get("target_label")
            .map_err(|_| admin_invalid("target_label"))?,
        request_id: row
            .try_get("request_id")
            .map_err(|_| admin_invalid("request_id"))?,
        started_at: row
            .try_get("started_at")
            .map_err(|_| admin_invalid("started_at"))?,
        finished_at: row
            .try_get("finished_at")
            .map_err(|_| admin_invalid("finished_at"))?,
        status_code: row
            .try_get::<Option<i32>, _>("status_code")
            .map_err(|_| admin_invalid("status_code"))?
            .and_then(|value| u16::try_from(value).ok()),
        http_version: row
            .try_get("http_version")
            .map_err(|_| admin_invalid("http_version"))?,
        outcome: row
            .try_get("outcome")
            .map_err(|_| admin_invalid("outcome"))?,
        latency_ms: nonnegative_u64(row, "latency_ms")?,
        request_header_count: nonnegative_u32(row, "request_header_count")?,
        response_header_count: nonnegative_u32(row, "response_header_count")?,
        request_header_bytes: nonnegative_u64(row, "request_header_bytes")?,
        response_header_bytes: nonnegative_u64(row, "response_header_bytes")?,
        request_turn_state: header_summary(
            request_count,
            request_length,
            row.try_get("request_turn_state_sha256")
                .map_err(|_| admin_invalid("request_turn_state_sha256"))?,
        ),
        response_turn_state: header_summary(
            response_count,
            response_length,
            row.try_get("response_turn_state_sha256")
                .map_err(|_| admin_invalid("response_turn_state_sha256"))?,
        ),
    })
}

fn header_summary(
    count: u32,
    length: Option<i32>,
    sha256: Option<String>,
) -> TurnStateHeaderSummary {
    TurnStateHeaderSummary {
        count,
        present: count > 0,
        byte_length: length.and_then(|value| u32::try_from(value).ok()),
        sha256,
        valid_292: length == Some(292),
    }
}

fn nonnegative_u64(row: &PgRow, field: &'static str) -> AdminStoreResult<u64> {
    row.try_get::<i64, _>(field)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| admin_invalid(field))
}

fn nonnegative_u32(row: &PgRow, field: &'static str) -> AdminStoreResult<u32> {
    row.try_get::<i32, _>(field)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| admin_invalid(field))
}

fn admin_unavailable(operation: &'static str) -> AdminStoreError {
    AdminStoreError::new(
        gateway_admin::ports::store::AdminStoreErrorKind::Unavailable,
        "turn state probe capture",
        operation,
    )
}

fn admin_invalid(field: &'static str) -> AdminStoreError {
    AdminStoreError::new(
        gateway_admin::ports::store::AdminStoreErrorKind::Invalid,
        "turn state probe capture",
        format!("invalid {field}"),
    )
}

fn saturating_increment(counter: &AtomicU64, increment: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(increment))
    });
}
