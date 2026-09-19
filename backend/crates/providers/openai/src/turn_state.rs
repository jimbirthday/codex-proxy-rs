//! 账号与模型级 Codex turn state 运行态；state 原文只在 Provider 内存中存在。

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Utc};
use gateway_admin::model::turn_state::{
    TurnStateOverviewEntry, TurnStateProbeResult, TurnStateProbeSubject, TurnStateProbeTarget,
    TurnStateSnapshot, TurnStateSource,
};
use gateway_core::{
    account::{CredentialRevision, OutboundProxy, ProviderAccountId},
    routing::UpstreamModelId,
};
use secrecy::{ExposeSecret, SecretString};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
const RENEW_BEFORE: Duration = Duration::from_secs(5 * 60);
const PROBE_INTERVAL: Duration = Duration::from_secs(10);
const PROBE_BUDGET_WINDOW: Duration = Duration::from_secs(5 * 60);
const PROBE_BUDGET_LIMIT: usize = 3;
const BUSINESS_ACTIVITY_WINDOW: Duration = Duration::from_secs(60 * 60);
const TURN_STATE_BYTES: usize = 292;
const MAX_ENTRIES: usize = 10_000;
const MAX_ACCOUNT_RECORDS: usize = 10_000;
const MAX_ACCOUNT_PROXY_RECORDS: usize = 10_000;
const MAX_MODEL_PROXY_RECORDS: usize = 10_000;
const MAX_PROBE_HISTORY: usize = 20;
const PROBE_REQUEST_CONCURRENCY: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TurnStateKey {
    account_id: ProviderAccountId,
    model: UpstreamModelId,
}

impl TurnStateKey {
    fn new(account_id: &ProviderAccountId, model: &UpstreamModelId) -> Self {
        Self {
            account_id: account_id.clone(),
            model: model.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AccountProxyKey {
    account_id: ProviderAccountId,
    proxy_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ModelProxyKey {
    subject: TurnStateKey,
    proxy_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProxyPreference {
    id: String,
    proxy: OutboundProxy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnStateVersion(u64);

#[derive(Clone)]
pub(crate) struct TurnStateStore {
    entries: Arc<Mutex<HashMap<TurnStateKey, Entry>>>,
    schedule: Arc<Mutex<Schedule>>,
    request_slots: Arc<Semaphore>,
}

struct Entry {
    state: Option<SecretString>,
    captured_at: Option<SystemTime>,
    first_applied_at: Option<SystemTime>,
    expires_at: Option<SystemTime>,
    source: Option<TurnStateSource>,
    probe_history: VecDeque<TurnStateProbeResult>,
    invalidated_at: Option<DateTime<Utc>>,
    invalidation_reason: Option<String>,
    version: TurnStateVersion,
    preferred_proxy: Option<ProxyPreference>,
    candidate_cursor: usize,
    failure_count: u64,
    failure_backoff_until: Option<Instant>,
    next_allowed_at: Option<Instant>,
    automatic_eligible: bool,
    last_business_activity_at: Option<Instant>,
    touched_at: SystemTime,
    schedule_touched_at: Instant,
}

impl Entry {
    fn empty(now: SystemTime, schedule_now: Instant, version: TurnStateVersion) -> Self {
        Self {
            state: None,
            captured_at: None,
            first_applied_at: None,
            expires_at: None,
            source: None,
            probe_history: VecDeque::new(),
            invalidated_at: None,
            invalidation_reason: None,
            version,
            preferred_proxy: None,
            candidate_cursor: 0,
            failure_count: 0,
            failure_backoff_until: None,
            next_allowed_at: None,
            automatic_eligible: false,
            last_business_activity_at: None,
            touched_at: now,
            schedule_touched_at: schedule_now,
        }
    }
}

struct Schedule {
    accounts: HashMap<ProviderAccountId, AccountSchedule>,
    account_proxies: HashMap<AccountProxyKey, ProxySchedule>,
    model_proxies: HashMap<ModelProxyKey, ProxySchedule>,
    next_version: u64,
    next_generation: u64,
    next_run_id: u64,
}

struct AccountSchedule {
    credential_revision: CredentialRevision,
    generation: u64,
    deleted: bool,
    running: Option<RunningProbe>,
    last_request_at: Option<Instant>,
    recent_requests: VecDeque<Instant>,
    rate_limit_failures: u64,
    rate_limit_until: Option<Instant>,
    preferred_proxy: Option<ProxyPreference>,
    touched_at: Instant,
}

struct RunningProbe {
    id: u64,
    subject: TurnStateKey,
    credential_revision: CredentialRevision,
    candidates: HashMap<String, OutboundProxy>,
    base_positions: HashMap<String, usize>,
    started: HashSet<String>,
}

struct ProxySchedule {
    proxy: OutboundProxy,
    failure_count: u64,
    cooldown_until: Option<Instant>,
    touched_at: Instant,
}

impl ProxySchedule {
    fn new(proxy: OutboundProxy, now: Instant) -> Self {
        Self {
            proxy,
            failure_count: 0,
            cooldown_until: None,
            touched_at: now,
        }
    }

    fn fail(&mut self, now: Instant) {
        self.failure_count = self.failure_count.saturating_add(1);
        self.cooldown_until = Some(now + proxy_failure_delay(self.failure_count));
        self.touched_at = now;
    }

    fn succeed(&mut self, now: Instant) {
        self.failure_count = 0;
        self.cooldown_until = None;
        self.touched_at = now;
    }
}

pub(crate) enum TurnStateProbeAdmission {
    Ready(TurnStateProbeRun),
    Busy,
    Deferred,
    NoCandidates,
    CapacityExhausted,
}

pub(crate) enum TurnStateProbeRequestAdmission {
    Ready(OwnedSemaphorePermit),
    Deferred { until: Instant },
    Rejected,
}

enum ProbeRequestCheck {
    Ready,
    Deferred { until: Instant },
    Rejected,
}

pub(crate) struct TurnStateProbeRun {
    store: TurnStateStore,
    subject: TurnStateKey,
    credential_revision: CredentialRevision,
    account_generation: u64,
    run_id: u64,
    expected_version: TurnStateVersion,
    trigger: TurnStateSource,
    candidates: Vec<TurnStateProbeTarget>,
    all_candidates: Vec<TurnStateProbeTarget>,
    finished: bool,
}

impl TurnStateProbeRun {
    pub(crate) fn candidates(&self) -> &[TurnStateProbeTarget] {
        &self.candidates
    }
}

impl Drop for TurnStateProbeRun {
    fn drop(&mut self) {
        if !self.finished {
            self.store.release_run(
                &self.subject.account_id,
                self.account_generation,
                self.run_id,
            );
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnStateProbeFailureKind {
    Network,
    Timeout,
    ProxyConfiguration,
    AccountAuthenticationRejected,
    HttpStatus(u16),
    MissingState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TurnStateProbeFailure {
    pub(crate) target_id: String,
    pub(crate) kind: TurnStateProbeFailureKind,
}

impl TurnStateStore {
    pub(crate) fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            schedule: Arc::new(Mutex::new(Schedule {
                accounts: HashMap::new(),
                account_proxies: HashMap::new(),
                model_proxies: HashMap::new(),
                next_version: 0,
                next_generation: 0,
                next_run_id: 0,
            })),
            request_slots: Arc::new(Semaphore::new(PROBE_REQUEST_CONCURRENCY)),
        }
    }

    pub(crate) fn state(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
    ) -> Option<String> {
        let now = SystemTime::now();
        let schedule_now = Instant::now();
        let mut entries = self.entries.lock().ok()?;
        let entry = entries.get_mut(&TurnStateKey::new(account_id, model))?;
        if entry.expires_at.is_none_or(|expires_at| expires_at <= now) {
            return None;
        }
        let state = entry
            .state
            .as_ref()
            .map(|state| state.expose_secret().to_owned());
        if state.is_some() {
            entry.touched_at = now;
            entry.schedule_touched_at = schedule_now;
        }
        state
    }

    pub(crate) fn mark_applied(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
        expected_state: &str,
    ) -> bool {
        let now = SystemTime::now();
        let schedule_now = Instant::now();
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        let Some(entry) = entries.get_mut(&TurnStateKey::new(account_id, model)) else {
            return false;
        };
        if entry
            .state
            .as_ref()
            .is_none_or(|state| state.expose_secret() != expected_state)
        {
            return false;
        }
        entry.first_applied_at.get_or_insert(now);
        entry.automatic_eligible = true;
        entry.last_business_activity_at = Some(schedule_now);
        entry.touched_at = now;
        entry.schedule_touched_at = schedule_now;
        true
    }

    pub(crate) fn is_valid_state(state: &str) -> bool {
        state.len() == TURN_STATE_BYTES && reqwest::header::HeaderValue::from_str(state).is_ok()
    }

    pub(crate) fn put(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
        state: String,
        source: TurnStateSource,
    ) -> Option<DateTime<Utc>> {
        if !Self::is_valid_state(&state) {
            return None;
        }
        let now = SystemTime::now();
        let schedule_now = Instant::now();
        let mut schedule = self.schedule.lock().expect("turn state mutex poisoned");
        let mut entries = self.entries.lock().expect("turn state mutex poisoned");
        let key = TurnStateKey::new(account_id, model);
        if !ensure_entry(&mut entries, &mut schedule, &key, now, schedule_now) {
            return None;
        }
        let version = allocate_version(&mut schedule);
        Some(write_state(
            entries.get_mut(&key).expect("entry was ensured"),
            state,
            source,
            now,
            schedule_now,
            version,
        ))
    }

    pub(crate) fn invalidate(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
        reason: &str,
    ) {
        let now = SystemTime::now();
        let schedule_now = Instant::now();
        let mut schedule = self.schedule.lock().expect("turn state mutex poisoned");
        let mut entries = self.entries.lock().expect("turn state mutex poisoned");
        let key = TurnStateKey::new(account_id, model);
        if !ensure_entry(&mut entries, &mut schedule, &key, now, schedule_now) {
            return;
        }
        let version = allocate_version(&mut schedule);
        invalidate_entry(
            entries.get_mut(&key).expect("entry was ensured"),
            reason,
            reason == "upstream_312",
            now,
            schedule_now,
            version,
        );
    }

    pub(crate) fn remove_account(&self, account_id: &ProviderAccountId) {
        let Ok(mut schedule) = self.schedule.lock() else {
            return;
        };
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let _ = allocate_version(&mut schedule);
        entries.retain(|key, _| &key.account_id != account_id);
        schedule
            .account_proxies
            .retain(|key, _| &key.account_id != account_id);
        schedule
            .model_proxies
            .retain(|key, _| &key.subject.account_id != account_id);
        if let Some(account) = schedule.accounts.get_mut(account_id) {
            if account.running.is_some() {
                // 删除期间保留原 gate 与代次，旧运行只能释放它，不能复活账号状态。
                account.deleted = true;
                account.preferred_proxy = None;
                account.rate_limit_failures = 0;
                account.rate_limit_until = None;
            } else {
                schedule.accounts.remove(account_id);
            }
        }
    }

    pub(crate) fn begin_probe(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
        credential_revision: CredentialRevision,
        targets: Vec<TurnStateProbeTarget>,
        bound_proxy: Option<&OutboundProxy>,
        trigger: TurnStateSource,
    ) -> TurnStateProbeAdmission {
        let now = SystemTime::now();
        let schedule_now = Instant::now();
        let subject = TurnStateKey::new(account_id, model);
        let mut schedule = self.schedule.lock().expect("turn state mutex poisoned");
        if schedule
            .accounts
            .get(account_id)
            .is_some_and(|account| account.running.is_some())
        {
            return TurnStateProbeAdmission::Busy;
        }
        let targets = normalize_targets(targets);
        if targets.is_empty() {
            return TurnStateProbeAdmission::NoCandidates;
        }
        let mut entries = self.entries.lock().expect("turn state mutex poisoned");
        if !ensure_account(&mut schedule, account_id, credential_revision, schedule_now) {
            return TurnStateProbeAdmission::CapacityExhausted;
        }
        synchronize_revision(
            &mut schedule,
            &mut entries,
            account_id,
            credential_revision,
            schedule_now,
        );
        synchronize_candidates(&mut schedule, &mut entries, account_id, &targets);
        if !ensure_entry(&mut entries, &mut schedule, &subject, now, schedule_now) {
            return TurnStateProbeAdmission::CapacityExhausted;
        }

        let entry_limit = entries
            .get(&subject)
            .and_then(|entry| entry.next_allowed_at);
        let account = schedule
            .accounts
            .get(account_id)
            .expect("account was ensured");
        let limit = later(
            later(entry_limit, account.rate_limit_until),
            budget_recovery(account, schedule_now),
        );
        if limit.is_some_and(|until| until > schedule_now) {
            return TurnStateProbeAdmission::Deferred;
        }

        if trigger == TurnStateSource::AutomaticRenewal
            && !automatic_due(
                entries.get(&subject).expect("entry was ensured"),
                now,
                schedule_now,
            )
        {
            return TurnStateProbeAdmission::Deferred;
        }
        let ordered = ordered_candidates(&schedule, &entries, &subject, &targets, bound_proxy);
        let mut available = Vec::new();
        let mut earliest_recovery = None;
        for target in &ordered {
            let cooldown = candidate_cooldown(&schedule, &subject, target);
            if cooldown.is_some_and(|until| until > schedule_now) {
                earliest_recovery = earlier(earliest_recovery, cooldown);
            } else {
                available.push(target.clone());
            }
        }
        if available.is_empty() {
            let until = earliest_recovery.expect("cooled candidate has recovery time");
            let entry = entries.get_mut(&subject).expect("entry was ensured");
            entry.next_allowed_at = later(entry.failure_backoff_until, Some(until));
            entry.schedule_touched_at = schedule_now;
            return TurnStateProbeAdmission::Deferred;
        }
        if !reserve_proxy_records(&mut schedule, &subject, &targets, schedule_now) {
            return TurnStateProbeAdmission::CapacityExhausted;
        }

        let run_id = allocate_run_id(&mut schedule);
        let account_generation = schedule
            .accounts
            .get(account_id)
            .expect("account was ensured")
            .generation;
        let expected_version = entries.get(&subject).expect("entry was ensured").version;
        let candidates = targets
            .iter()
            .map(|target| (target.id.clone(), target.proxy.clone()))
            .collect();
        let base_positions = targets
            .iter()
            .enumerate()
            .map(|(index, target)| (target.id.clone(), index))
            .collect();
        let account = schedule
            .accounts
            .get_mut(account_id)
            .expect("account was ensured");
        account.running = Some(RunningProbe {
            id: run_id,
            subject: subject.clone(),
            credential_revision,
            candidates,
            base_positions,
            started: HashSet::new(),
        });
        account.touched_at = schedule_now;
        TurnStateProbeAdmission::Ready(TurnStateProbeRun {
            store: self.clone(),
            subject,
            credential_revision,
            account_generation,
            run_id,
            expected_version,
            trigger,
            candidates: available,
            all_candidates: targets,
            finished: false,
        })
    }

    pub(crate) async fn start_probe_request(
        &self,
        run: &TurnStateProbeRun,
        target: &TurnStateProbeTarget,
    ) -> TurnStateProbeRequestAdmission {
        let first_check = {
            let Ok(schedule) = self.schedule.lock() else {
                return TurnStateProbeRequestAdmission::Rejected;
            };
            let Ok(entries) = self.entries.lock() else {
                return TurnStateProbeRequestAdmission::Rejected;
            };
            check_probe_request(&schedule, &entries, run, target, Instant::now())
        };
        match first_check {
            ProbeRequestCheck::Ready => {}
            ProbeRequestCheck::Deferred { until } => {
                return TurnStateProbeRequestAdmission::Deferred { until };
            }
            ProbeRequestCheck::Rejected => return TurnStateProbeRequestAdmission::Rejected,
        }

        // 调用方在 permit 外等待并重试；Store 本身不持锁跨 await，也不主动 sleep。
        let Ok(permit) = self.request_slots.clone().acquire_owned().await else {
            return TurnStateProbeRequestAdmission::Rejected;
        };
        let now = Instant::now();
        let Ok(mut schedule) = self.schedule.lock() else {
            return TurnStateProbeRequestAdmission::Rejected;
        };
        let Ok(mut entries) = self.entries.lock() else {
            return TurnStateProbeRequestAdmission::Rejected;
        };
        match check_probe_request(&schedule, &entries, run, target, now) {
            ProbeRequestCheck::Ready => {}
            ProbeRequestCheck::Deferred { until } => {
                return TurnStateProbeRequestAdmission::Deferred { until };
            }
            ProbeRequestCheck::Rejected => return TurnStateProbeRequestAdmission::Rejected,
        }

        let account = schedule
            .accounts
            .get_mut(&run.subject.account_id)
            .expect("request admission checked account");
        let running = account
            .running
            .as_mut()
            .expect("request admission checked run");
        let next_cursor = running
            .base_positions
            .get(&target.id)
            .map(|index| (index + 1) % running.base_positions.len().max(1));
        let inserted = running.started.insert(target.id.clone());
        debug_assert!(inserted, "request admission checked duplicate target");
        account
            .recent_requests
            .retain(|at| *at + PROBE_BUDGET_WINDOW > now);
        account.recent_requests.push_back(now);
        account.last_request_at = Some(now);
        account.touched_at = now;
        if let Some(next_cursor) = next_cursor {
            let entry = entries
                .get_mut(&run.subject)
                .expect("request admission checked entry");
            entry.candidate_cursor = next_cursor;
            entry.schedule_touched_at = now;
        }
        TurnStateProbeRequestAdmission::Ready(permit)
    }

    pub(crate) fn finish_probe(
        &self,
        run: &mut TurnStateProbeRun,
        mut result: TurnStateProbeResult,
        state: Option<String>,
        failures: &[TurnStateProbeFailure],
        current_facts_match: bool,
    ) -> TurnStateProbeResult {
        let requested_active_target_id = result.active_target_id.take();
        result.state_expires_at = None;
        let now = SystemTime::now();
        let schedule_now = Instant::now();
        let mut schedule = self.schedule.lock().expect("turn state mutex poisoned");
        let Some(account) = schedule.accounts.get(&run.subject.account_id) else {
            run.finished = true;
            return result;
        };
        let Some(running) = account.running.as_ref() else {
            run.finished = true;
            return result;
        };
        if account.generation != run.account_generation
            || account.credential_revision != run.credential_revision
            || running.id != run.run_id
            || running.subject != run.subject
        {
            run.finished = true;
            return result;
        }
        if account.deleted {
            let account = schedule
                .accounts
                .get_mut(&run.subject.account_id)
                .expect("validated deleted account exists");
            account.running = None;
            account.touched_at = schedule_now;
            run.finished = true;
            return result;
        }
        let started = running.started.clone();
        let run_candidates = running.candidates.clone();
        let mut entries = self.entries.lock().expect("turn state mutex poisoned");
        let version_matches = current_facts_match
            && entries.get(&run.subject).map(|entry| entry.version) == Some(run.expected_version);
        let active_target = requested_active_target_id.as_ref().and_then(|id| {
            run.all_candidates
                .iter()
                .find(|target| {
                    &target.id == id
                        && started.contains(id)
                        && run_candidates.get(id) == Some(&target.proxy)
                })
                .cloned()
        });
        let valid_state = state.filter(|state| Self::is_valid_state(state));
        let mut saw_312 = false;

        for failure in failures.iter().filter(|_| current_facts_match) {
            let Some(target) = run.all_candidates.iter().find(|target| {
                target.id == failure.target_id
                    && run_candidates.get(&failure.target_id) == Some(&target.proxy)
            }) else {
                continue;
            };
            if failure.kind != TurnStateProbeFailureKind::ProxyConfiguration
                && !started.contains(&failure.target_id)
            {
                continue;
            }
            saw_312 |= failure.kind == TurnStateProbeFailureKind::HttpStatus(312);
            apply_proxy_failure(
                &mut schedule,
                &run.subject,
                target,
                failure.kind,
                schedule_now,
            );
            if failure.kind == TurnStateProbeFailureKind::HttpStatus(429)
                && started.contains(&failure.target_id)
            {
                let account = schedule
                    .accounts
                    .get_mut(&run.subject.account_id)
                    .expect("validated account exists");
                account.rate_limit_failures = account.rate_limit_failures.saturating_add(1);
                account.rate_limit_until = later(
                    account.rate_limit_until,
                    Some(
                        schedule_now
                            + backoff_delay(
                                account.rate_limit_failures,
                                run.subject.account_id.as_str().as_bytes(),
                                None,
                            ),
                    ),
                );
            }
        }

        let committed_target = if version_matches {
            active_target
                .as_ref()
                .zip(valid_state)
                .map(|(target, state)| {
                    let version = allocate_version(&mut schedule);
                    let entry = entries
                        .get_mut(&run.subject)
                        .expect("versioned entry exists");
                    let expires_at =
                        write_state(entry, state, run.trigger, now, schedule_now, version);
                    result.active_target_id = Some(target.id.clone());
                    result.state_expires_at = Some(expires_at);
                    target.clone()
                })
        } else {
            None
        };
        let probe_succeeded = committed_target.is_some();

        if let Some(target) = committed_target.as_ref() {
            let preference = ProxyPreference {
                id: target.id.clone(),
                proxy: target.proxy.clone(),
            };
            clear_proxy_cooldowns(&mut schedule, &run.subject, target, schedule_now);
            if let Some(account) = schedule.accounts.get_mut(&run.subject.account_id) {
                account.preferred_proxy = Some(preference.clone());
                account.rate_limit_failures = 0;
                account.touched_at = schedule_now;
            }
            if let Some(entry) = entries.get_mut(&run.subject) {
                entry.preferred_proxy = Some(preference);
            }
        } else if saw_312 && version_matches {
            let version = allocate_version(&mut schedule);
            invalidate_entry(
                entries
                    .get_mut(&run.subject)
                    .expect("versioned entry exists"),
                "probe_upstream_312",
                false,
                now,
                schedule_now,
                version,
            );
        }

        if version_matches && let Some(entry) = entries.get_mut(&run.subject) {
            if probe_succeeded {
                entry.failure_count = 0;
                entry.failure_backoff_until = None;
                entry.next_allowed_at = None;
            } else if !started.is_empty() {
                entry.failure_count = entry.failure_count.saturating_add(1);
                entry.failure_backoff_until = Some(
                    schedule_now
                        + backoff_delay(
                            entry.failure_count,
                            run.subject.account_id.as_str().as_bytes(),
                            Some(run.subject.model.as_str().as_bytes()),
                        ),
                );
            }
            entry.schedule_touched_at = schedule_now;
        }
        let recovery =
            all_candidates_cooldown(&schedule, &run.subject, &run.all_candidates, schedule_now);
        if version_matches && let Some(entry) = entries.get_mut(&run.subject) {
            entry.next_allowed_at = recovery
                .map(|until| later(entry.failure_backoff_until, Some(until)))
                .unwrap_or(entry.failure_backoff_until);
        }
        if !result.attempts.is_empty()
            && let Some(entry) = entries.get_mut(&run.subject)
        {
            // 历史保存实际响应事实；成功出口字段已经按条件提交结果规范化。
            entry.probe_history.push_front(result.clone());
            entry.probe_history.truncate(MAX_PROBE_HISTORY);
            entry.touched_at = now;
            entry.schedule_touched_at = schedule_now;
        }
        if let Some(account) = schedule.accounts.get_mut(&run.subject.account_id) {
            account.running = None;
            account.touched_at = schedule_now;
        }
        run.finished = true;
        result
    }

    pub(crate) fn snapshot(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
    ) -> Option<TurnStateSnapshot> {
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(&TurnStateKey::new(account_id, model))?;
        let expires_at = entry
            .expires_at
            .filter(|expires_at| *expires_at > SystemTime::now())
            .map(DateTime::<Utc>::from);
        Some(TurnStateSnapshot {
            account_id: account_id.as_str().to_owned(),
            model: model.as_str().to_owned(),
            state_captured_at: entry.captured_at.map(DateTime::<Utc>::from),
            state_first_applied_at: entry.first_applied_at.map(DateTime::<Utc>::from),
            state_expires_at: expires_at,
            next_rotation_at: entry
                .expires_at
                .filter(|expires_at| *expires_at > SystemTime::now())
                .map(|expires_at| DateTime::<Utc>::from(expires_at - RENEW_BEFORE)),
            state_source: entry.source,
            probe_history: entry.probe_history.iter().cloned().collect(),
            invalidated_at: entry.invalidated_at,
            invalidation_reason: entry.invalidation_reason.clone(),
        })
    }

    pub(crate) fn due_subjects(&self) -> Vec<TurnStateProbeSubject> {
        let Ok(schedule) = self.schedule.lock() else {
            return Vec::new();
        };
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        let schedule_now = Instant::now();
        let now = SystemTime::now();
        entries
            .iter()
            .filter(|(key, entry)| {
                let account_ready = schedule
                    .accounts
                    .get(&key.account_id)
                    .is_none_or(|account| {
                        !account.deleted
                            && account.running.is_none()
                            && account
                                .rate_limit_until
                                .is_none_or(|until| until <= schedule_now)
                            && account
                                .last_request_at
                                .is_none_or(|at| at + PROBE_INTERVAL <= schedule_now)
                            && budget_recovery(account, schedule_now).is_none()
                    });
                automatic_due(entry, now, schedule_now)
                    && entry
                        .next_allowed_at
                        .is_none_or(|until| until <= schedule_now)
                    && account_ready
            })
            .map(|(key, _)| TurnStateProbeSubject {
                account_id: key.account_id.clone(),
                model: key.model.clone(),
            })
            .collect()
    }

    pub(crate) fn automatic_due(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
    ) -> bool {
        self.entries.lock().ok().is_some_and(|entries| {
            entries
                .get(&TurnStateKey::new(account_id, model))
                .is_some_and(|entry| automatic_due(entry, SystemTime::now(), Instant::now()))
        })
    }

    pub(crate) fn overview(&self) -> Vec<TurnStateOverviewEntry> {
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        let now = SystemTime::now();
        let mut result = entries
            .iter()
            .filter_map(|(key, entry)| {
                let latest_probe = entry.probe_history.front();
                if entry.state.is_none()
                    && latest_probe.is_none()
                    && entry.invalidation_reason.is_none()
                {
                    return None;
                }
                let active_target = latest_probe.and_then(|probe| {
                    let id = probe.active_target_id.as_ref()?;
                    let label = probe
                        .attempts
                        .iter()
                        .find(|attempt| &attempt.target_id == id)
                        .map(|attempt| attempt.target_label.clone());
                    Some((id.clone(), label))
                });
                let state_available = entry.state.is_some()
                    && entry.expires_at.is_some_and(|expires_at| expires_at > now);
                Some(TurnStateOverviewEntry {
                    account_id: key.account_id.as_str().to_owned(),
                    model: key.model.as_str().to_owned(),
                    state_available,
                    state_captured_at: entry.captured_at.map(DateTime::<Utc>::from),
                    state_first_applied_at: entry.first_applied_at.map(DateTime::<Utc>::from),
                    state_expires_at: entry
                        .expires_at
                        .filter(|expires_at| *expires_at > now)
                        .map(DateTime::<Utc>::from),
                    next_rotation_at: entry
                        .expires_at
                        .filter(|expires_at| *expires_at > now)
                        .map(|expires_at| DateTime::<Utc>::from(expires_at - RENEW_BEFORE)),
                    state_source: entry.source,
                    latest_probe_at: latest_probe.map(|probe| probe.finished_at),
                    latest_probe_succeeded: latest_probe
                        .is_some_and(|probe| probe.active_target_id.is_some()),
                    latest_probe_active_target_id: active_target
                        .as_ref()
                        .map(|value| value.0.clone()),
                    latest_probe_active_target_label: active_target.and_then(|value| value.1),
                    latest_probe_attempt_count: latest_probe
                        .map_or(0, |probe| probe.attempts.len()),
                    invalidated_at: entry.invalidated_at,
                    invalidation_reason: entry.invalidation_reason.clone(),
                })
            })
            .collect::<Vec<_>>();
        result.sort_by(|left, right| {
            left.account_id
                .cmp(&right.account_id)
                .then_with(|| left.model.cmp(&right.model))
        });
        result
    }

    fn release_run(&self, account_id: &ProviderAccountId, generation: u64, run_id: u64) {
        let Ok(mut schedule) = self.schedule.lock() else {
            return;
        };
        let Some(account) = schedule.accounts.get_mut(account_id) else {
            return;
        };
        if account.generation == generation
            && account.running.as_ref().is_some_and(|run| run.id == run_id)
        {
            account.running = None;
            account.touched_at = Instant::now();
        }
    }
}

fn automatic_due(entry: &Entry, now: SystemTime, schedule_now: Instant) -> bool {
    entry.automatic_eligible
        && entry
            .last_business_activity_at
            .is_some_and(|at| at + BUSINESS_ACTIVITY_WINDOW > schedule_now)
        && (entry.expires_at.is_some_and(|at| at <= now + RENEW_BEFORE)
            || entry.invalidation_reason.is_some())
}

fn budget_recovery(account: &AccountSchedule, now: Instant) -> Option<Instant> {
    let mut recent = account
        .recent_requests
        .iter()
        .copied()
        .filter(|at| *at + PROBE_BUDGET_WINDOW > now);
    let oldest = recent.next()?;
    (recent.count() + 1 >= PROBE_BUDGET_LIMIT).then_some(oldest + PROBE_BUDGET_WINDOW)
}

fn write_state(
    entry: &mut Entry,
    state: String,
    source: TurnStateSource,
    now: SystemTime,
    schedule_now: Instant,
    version: TurnStateVersion,
) -> DateTime<Utc> {
    let expires_at = now + DEFAULT_TTL;
    let changed = entry
        .state
        .as_ref()
        .is_none_or(|current| current.expose_secret() != state);
    entry.state = Some(SecretString::from(state));
    entry.captured_at = Some(now);
    entry.expires_at = Some(expires_at);
    if changed {
        entry.first_applied_at = None;
        entry.automatic_eligible = false;
        entry.source = Some(source);
    }
    entry.invalidated_at = None;
    entry.invalidation_reason = None;
    entry.version = version;
    entry.failure_count = 0;
    entry.failure_backoff_until = None;
    entry.next_allowed_at = None;
    entry.touched_at = now;
    entry.schedule_touched_at = schedule_now;
    DateTime::<Utc>::from(expires_at)
}

fn invalidate_entry(
    entry: &mut Entry,
    reason: &str,
    automatic_eligible: bool,
    now: SystemTime,
    schedule_now: Instant,
    version: TurnStateVersion,
) {
    entry.state = None;
    entry.captured_at = None;
    entry.first_applied_at = None;
    entry.expires_at = None;
    entry.source = None;
    entry.invalidated_at = Some(Utc::now());
    entry.invalidation_reason = Some(reason.to_owned());
    entry.version = version;
    entry.automatic_eligible |= automatic_eligible;
    if automatic_eligible {
        entry.last_business_activity_at = Some(schedule_now);
    }
    entry.touched_at = now;
    entry.schedule_touched_at = schedule_now;
}

fn allocate_version(schedule: &mut Schedule) -> TurnStateVersion {
    schedule.next_version = schedule.next_version.wrapping_add(1).max(1);
    TurnStateVersion(schedule.next_version)
}

fn allocate_generation(schedule: &mut Schedule) -> u64 {
    schedule.next_generation = schedule.next_generation.wrapping_add(1).max(1);
    schedule.next_generation
}

fn allocate_run_id(schedule: &mut Schedule) -> u64 {
    schedule.next_run_id = schedule.next_run_id.wrapping_add(1).max(1);
    schedule.next_run_id
}

fn ensure_entry(
    entries: &mut HashMap<TurnStateKey, Entry>,
    schedule: &mut Schedule,
    key: &TurnStateKey,
    now: SystemTime,
    schedule_now: Instant,
) -> bool {
    if schedule
        .accounts
        .get(&key.account_id)
        .is_some_and(|account| account.deleted)
    {
        return false;
    }
    if entries.contains_key(key) {
        return true;
    }
    if entries.len() >= MAX_ENTRIES {
        let oldest = entries
            .iter()
            .filter(|(candidate, _)| {
                schedule
                    .accounts
                    .get(&candidate.account_id)
                    .is_none_or(|account| account.running.is_none())
            })
            .min_by_key(|(_, entry)| entry.touched_at)
            .map(|(key, _)| key.clone());
        let Some(oldest) = oldest else {
            return false;
        };
        entries.remove(&oldest);
    }
    if entries.len() >= MAX_ENTRIES {
        return false;
    }
    let version = allocate_version(schedule);
    entries.insert(key.clone(), Entry::empty(now, schedule_now, version));
    true
}

fn ensure_account(
    schedule: &mut Schedule,
    account_id: &ProviderAccountId,
    credential_revision: CredentialRevision,
    now: Instant,
) -> bool {
    if let Some(account) = schedule.accounts.get(account_id) {
        if !account.deleted {
            return true;
        }
        if account.running.is_some() {
            return false;
        }
        let last_request_at = account.last_request_at;
        let recent_requests = account.recent_requests.clone();
        let generation = allocate_generation(schedule);
        schedule.accounts.insert(
            account_id.clone(),
            AccountSchedule {
                credential_revision,
                generation,
                deleted: false,
                running: None,
                last_request_at,
                recent_requests,
                rate_limit_failures: 0,
                rate_limit_until: None,
                preferred_proxy: None,
                touched_at: now,
            },
        );
        return true;
    }
    if schedule.accounts.len() >= MAX_ACCOUNT_RECORDS {
        let oldest = schedule
            .accounts
            .iter()
            .filter(|(_, account)| {
                account.running.is_none()
                    && account
                        .recent_requests
                        .back()
                        .is_none_or(|at| *at + PROBE_BUDGET_WINDOW <= now)
            })
            .min_by_key(|(_, account)| account.touched_at)
            .map(|(id, _)| id.clone());
        let Some(oldest) = oldest else {
            return false;
        };
        schedule.accounts.remove(&oldest);
        schedule
            .account_proxies
            .retain(|key, _| key.account_id != oldest);
        schedule
            .model_proxies
            .retain(|key, _| key.subject.account_id != oldest);
    }
    if schedule.accounts.len() >= MAX_ACCOUNT_RECORDS {
        return false;
    }
    let generation = allocate_generation(schedule);
    schedule.accounts.insert(
        account_id.clone(),
        AccountSchedule {
            credential_revision,
            generation,
            deleted: false,
            running: None,
            last_request_at: None,
            recent_requests: VecDeque::new(),
            rate_limit_failures: 0,
            rate_limit_until: None,
            preferred_proxy: None,
            touched_at: now,
        },
    );
    true
}

fn synchronize_revision(
    schedule: &mut Schedule,
    entries: &mut HashMap<TurnStateKey, Entry>,
    account_id: &ProviderAccountId,
    revision: CredentialRevision,
    now: Instant,
) {
    let changed = schedule
        .accounts
        .get(account_id)
        .is_some_and(|account| account.credential_revision != revision);
    if !changed {
        return;
    }
    if let Some(account) = schedule.accounts.get_mut(account_id) {
        // credential 更新不解除正在运行的 gate，也不抹掉已发生的十秒间隔。
        account.credential_revision = revision;
        account.rate_limit_failures = 0;
        account.rate_limit_until = None;
        account.preferred_proxy = None;
        account.touched_at = now;
    }
    for (key, entry) in entries {
        if &key.account_id == account_id {
            entry.preferred_proxy = None;
            entry.candidate_cursor = 0;
            entry.failure_count = 0;
            entry.failure_backoff_until = None;
            entry.next_allowed_at = None;
            entry.schedule_touched_at = now;
        }
    }
    schedule
        .account_proxies
        .retain(|key, _| &key.account_id != account_id);
    schedule
        .model_proxies
        .retain(|key, _| &key.subject.account_id != account_id);
}

fn check_probe_request(
    schedule: &Schedule,
    entries: &HashMap<TurnStateKey, Entry>,
    run: &TurnStateProbeRun,
    target: &TurnStateProbeTarget,
    now: Instant,
) -> ProbeRequestCheck {
    let Some(account) = schedule.accounts.get(&run.subject.account_id) else {
        return ProbeRequestCheck::Rejected;
    };
    if account.deleted
        || account.generation != run.account_generation
        || account.credential_revision != run.credential_revision
    {
        return ProbeRequestCheck::Rejected;
    }
    let Some(running) = account.running.as_ref() else {
        return ProbeRequestCheck::Rejected;
    };
    if running.id != run.run_id
        || running.subject != run.subject
        || running.credential_revision != run.credential_revision
        || running.candidates.get(&target.id) != Some(&target.proxy)
        || running.started.contains(&target.id)
        || entries.get(&run.subject).map(|entry| entry.version) != Some(run.expected_version)
    {
        return ProbeRequestCheck::Rejected;
    }
    // 预算耗尽就结束本轮，不持有账号 gate 等待下一个五分钟窗口。
    if budget_recovery(account, now).is_some()
        || account.rate_limit_until.is_some_and(|until| until > now)
        || (run.trigger == TurnStateSource::AutomaticRenewal
            && !entries
                .get(&run.subject)
                .is_some_and(|entry| automatic_due(entry, SystemTime::now(), now)))
    {
        return ProbeRequestCheck::Rejected;
    }
    if let Some(until) = account
        .last_request_at
        .map(|at| at + PROBE_INTERVAL)
        .filter(|until| *until > now)
    {
        return ProbeRequestCheck::Deferred { until };
    }
    ProbeRequestCheck::Ready
}

fn normalize_targets(targets: Vec<TurnStateProbeTarget>) -> Vec<TurnStateProbeTarget> {
    let mut sorted = BTreeMap::new();
    for target in targets {
        sorted.entry(target.id.clone()).or_insert(target);
    }
    sorted.into_values().collect()
}

fn synchronize_candidates(
    schedule: &mut Schedule,
    entries: &mut HashMap<TurnStateKey, Entry>,
    account_id: &ProviderAccountId,
    targets: &[TurnStateProbeTarget],
) {
    let current = targets
        .iter()
        .map(|target| (target.id.as_str(), &target.proxy))
        .collect::<HashMap<_, _>>();
    schedule.account_proxies.retain(|key, record| {
        &key.account_id != account_id
            || current
                .get(key.proxy_id.as_str())
                .is_some_and(|proxy| *proxy == &record.proxy)
    });
    schedule.model_proxies.retain(|key, record| {
        &key.subject.account_id != account_id
            || current
                .get(key.proxy_id.as_str())
                .is_some_and(|proxy| *proxy == &record.proxy)
    });
    if let Some(account) = schedule.accounts.get_mut(account_id)
        && account.preferred_proxy.as_ref().is_some_and(|preferred| {
            current
                .get(preferred.id.as_str())
                .is_none_or(|proxy| *proxy != &preferred.proxy)
        })
    {
        account.preferred_proxy = None;
    }
    for (key, entry) in entries {
        if &key.account_id == account_id
            && entry.preferred_proxy.as_ref().is_some_and(|preferred| {
                current
                    .get(preferred.id.as_str())
                    .is_none_or(|proxy| *proxy != &preferred.proxy)
            })
        {
            entry.preferred_proxy = None;
        }
    }
}

fn ordered_candidates(
    schedule: &Schedule,
    entries: &HashMap<TurnStateKey, Entry>,
    subject: &TurnStateKey,
    targets: &[TurnStateProbeTarget],
    bound_proxy: Option<&OutboundProxy>,
) -> Vec<TurnStateProbeTarget> {
    let by_id = targets
        .iter()
        .map(|target| (target.id.as_str(), target))
        .collect::<HashMap<_, _>>();
    let mut result = Vec::with_capacity(targets.len());
    let mut seen = HashSet::new();
    for target in targets
        .iter()
        .filter(|target| Some(&target.proxy) == bound_proxy)
    {
        if seen.insert(target.id.as_str()) {
            result.push(target.clone());
        }
    }
    let preferences = [
        entries
            .get(subject)
            .and_then(|entry| entry.preferred_proxy.as_ref()),
        schedule
            .accounts
            .get(&subject.account_id)
            .and_then(|account| account.preferred_proxy.as_ref()),
    ];
    for preferred in preferences.into_iter().flatten() {
        if let Some(target) = by_id.get(preferred.id.as_str())
            && target.proxy == preferred.proxy
            && seen.insert(target.id.as_str())
        {
            result.push((*target).clone());
        }
    }
    let cursor = entries
        .get(subject)
        .map_or(0, |entry| entry.candidate_cursor % targets.len());
    for offset in 0..targets.len() {
        let target = &targets[(cursor + offset) % targets.len()];
        if seen.insert(target.id.as_str()) {
            result.push(target.clone());
        }
    }
    result
}

fn reserve_proxy_records(
    schedule: &mut Schedule,
    subject: &TurnStateKey,
    targets: &[TurnStateProbeTarget],
    now: Instant,
) -> bool {
    let missing_account = targets
        .iter()
        .filter(|target| {
            !schedule.account_proxies.contains_key(&AccountProxyKey {
                account_id: subject.account_id.clone(),
                proxy_id: target.id.clone(),
            })
        })
        .count();
    while schedule.account_proxies.len() + missing_account > MAX_ACCOUNT_PROXY_RECORDS {
        let oldest = schedule
            .account_proxies
            .iter()
            .filter(|(key, _)| key.account_id != subject.account_id)
            .filter(|(key, _)| {
                schedule
                    .accounts
                    .get(&key.account_id)
                    .is_none_or(|account| account.running.is_none())
            })
            .min_by_key(|(_, record)| record.touched_at)
            .map(|(key, _)| key.clone());
        let Some(oldest) = oldest else {
            return false;
        };
        schedule.account_proxies.remove(&oldest);
    }
    let missing_model = targets
        .iter()
        .filter(|target| {
            !schedule.model_proxies.contains_key(&ModelProxyKey {
                subject: subject.clone(),
                proxy_id: target.id.clone(),
            })
        })
        .count();
    while schedule.model_proxies.len() + missing_model > MAX_MODEL_PROXY_RECORDS {
        let oldest = schedule
            .model_proxies
            .iter()
            .filter(|(key, _)| key.subject != *subject)
            .filter(|(key, _)| {
                schedule
                    .accounts
                    .get(&key.subject.account_id)
                    .is_none_or(|account| account.running.is_none())
            })
            .min_by_key(|(_, record)| record.touched_at)
            .map(|(key, _)| key.clone());
        let Some(oldest) = oldest else {
            return false;
        };
        schedule.model_proxies.remove(&oldest);
    }
    for target in targets {
        schedule
            .account_proxies
            .entry(AccountProxyKey {
                account_id: subject.account_id.clone(),
                proxy_id: target.id.clone(),
            })
            .or_insert_with(|| ProxySchedule::new(target.proxy.clone(), now));
        schedule
            .model_proxies
            .entry(ModelProxyKey {
                subject: subject.clone(),
                proxy_id: target.id.clone(),
            })
            .or_insert_with(|| ProxySchedule::new(target.proxy.clone(), now));
    }
    true
}

fn candidate_cooldown(
    schedule: &Schedule,
    subject: &TurnStateKey,
    target: &TurnStateProbeTarget,
) -> Option<Instant> {
    let account = schedule
        .account_proxies
        .get(&AccountProxyKey {
            account_id: subject.account_id.clone(),
            proxy_id: target.id.clone(),
        })
        .filter(|record| record.proxy == target.proxy)
        .and_then(|record| record.cooldown_until);
    let model = schedule
        .model_proxies
        .get(&ModelProxyKey {
            subject: subject.clone(),
            proxy_id: target.id.clone(),
        })
        .filter(|record| record.proxy == target.proxy)
        .and_then(|record| record.cooldown_until);
    later(account, model)
}

fn all_candidates_cooldown(
    schedule: &Schedule,
    subject: &TurnStateKey,
    targets: &[TurnStateProbeTarget],
    now: Instant,
) -> Option<Instant> {
    let mut earliest = None;
    for target in targets {
        let until = candidate_cooldown(schedule, subject, target).filter(|at| *at > now)?;
        earliest = earlier(earliest, Some(until));
    }
    earliest
}

fn apply_proxy_failure(
    schedule: &mut Schedule,
    subject: &TurnStateKey,
    target: &TurnStateProbeTarget,
    kind: TurnStateProbeFailureKind,
    now: Instant,
) {
    if matches!(
        kind,
        TurnStateProbeFailureKind::AccountAuthenticationRejected
            | TurnStateProbeFailureKind::HttpStatus(429)
    ) {
        return;
    }
    if matches!(
        kind,
        TurnStateProbeFailureKind::Network
            | TurnStateProbeFailureKind::Timeout
            | TurnStateProbeFailureKind::ProxyConfiguration
            | TurnStateProbeFailureKind::HttpStatus(407)
    ) {
        if let Some(record) = schedule.account_proxies.get_mut(&AccountProxyKey {
            account_id: subject.account_id.clone(),
            proxy_id: target.id.clone(),
        }) && record.proxy == target.proxy
        {
            record.fail(now);
        }
    } else if let Some(record) = schedule.model_proxies.get_mut(&ModelProxyKey {
        subject: subject.clone(),
        proxy_id: target.id.clone(),
    }) && record.proxy == target.proxy
    {
        record.fail(now);
    }
}

fn clear_proxy_cooldowns(
    schedule: &mut Schedule,
    subject: &TurnStateKey,
    target: &TurnStateProbeTarget,
    now: Instant,
) {
    if let Some(record) = schedule.account_proxies.get_mut(&AccountProxyKey {
        account_id: subject.account_id.clone(),
        proxy_id: target.id.clone(),
    }) && record.proxy == target.proxy
    {
        record.succeed(now);
    }
    if let Some(record) = schedule.model_proxies.get_mut(&ModelProxyKey {
        subject: subject.clone(),
        proxy_id: target.id.clone(),
    }) && record.proxy == target.proxy
    {
        record.succeed(now);
    }
}

fn backoff_delay(failure_count: u64, account_id: &[u8], model: Option<&[u8]>) -> Duration {
    let exponent = failure_count.saturating_sub(1).min(4) as u32;
    let base = (120_u64.saturating_mul(1_u64 << exponent)).min(1_800);
    let mut seed = 0_u64;
    for byte in account_id {
        seed = seed.wrapping_mul(31).wrapping_add(u64::from(*byte));
    }
    if let Some(model) = model {
        // 零分隔符也参与同一 wrapping 哈希，避免账号和模型边界歧义。
        seed = seed.wrapping_mul(31);
        for byte in model {
            seed = seed.wrapping_mul(31).wrapping_add(u64::from(*byte));
        }
    }
    let jitter = seed.wrapping_add(failure_count.wrapping_mul(17)) % 31;
    Duration::from_secs(base.saturating_add(jitter).min(1_800))
}

fn proxy_failure_delay(failure_count: u64) -> Duration {
    let exponent = failure_count.saturating_sub(1).min(3) as u32;
    Duration::from_secs((300_u64.saturating_mul(1_u64 << exponent)).min(1_800))
}

fn later(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn earlier(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}
