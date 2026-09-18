//! 账号与模型级 Codex turn state 运行态；state 原文只在 Provider 内存中存在。

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Utc};
use gateway_admin::model::turn_state::{
    TurnStateOverviewEntry, TurnStateProbeResult, TurnStateProbeSubject, TurnStateSnapshot,
    TurnStateSource,
};
use gateway_core::{account::ProviderAccountId, routing::UpstreamModelId};
use secrecy::{ExposeSecret, SecretString};

const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
const RENEW_BEFORE: Duration = Duration::from_secs(5 * 60);
const TURN_STATE_BYTES: usize = 292;
const MAX_ENTRIES: usize = 10_000;
const MAX_PROBE_HISTORY: usize = 20;

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

#[derive(Clone)]
pub(crate) struct TurnStateStore {
    entries: Arc<Mutex<HashMap<TurnStateKey, Entry>>>,
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
    touched_at: SystemTime,
}

impl Entry {
    fn empty(now: SystemTime) -> Self {
        Self {
            state: None,
            captured_at: None,
            first_applied_at: None,
            expires_at: None,
            source: None,
            probe_history: VecDeque::new(),
            invalidated_at: None,
            invalidation_reason: None,
            touched_at: now,
        }
    }
}

impl TurnStateStore {
    pub(crate) fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn state(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
    ) -> Option<String> {
        let now = SystemTime::now();
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
        entry.touched_at = now;
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
        let expires_at = now + DEFAULT_TTL;
        let expires_at_utc = DateTime::<Utc>::from(expires_at);
        let mut entries = self.entries.lock().expect("turn state mutex poisoned");
        let entry = entry_mut(&mut entries, TurnStateKey::new(account_id, model), now);
        let state_changed = entry
            .state
            .as_ref()
            .is_none_or(|current| current.expose_secret() != state);
        entry.state = Some(SecretString::from(state));
        entry.captured_at = Some(now);
        entry.expires_at = Some(expires_at);
        if state_changed {
            entry.first_applied_at = None;
            entry.source = Some(source);
        }
        entry.invalidated_at = None;
        entry.invalidation_reason = None;
        entry.touched_at = now;
        Some(expires_at_utc)
    }

    pub(crate) fn record_probe(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
        result: TurnStateProbeResult,
    ) {
        let now = SystemTime::now();
        let mut entries = self.entries.lock().expect("turn state mutex poisoned");
        let entry = entry_mut(&mut entries, TurnStateKey::new(account_id, model), now);
        entry.probe_history.push_front(result);
        entry.probe_history.truncate(MAX_PROBE_HISTORY);
        entry.touched_at = now;
    }

    pub(crate) fn invalidate(
        &self,
        account_id: &ProviderAccountId,
        model: &UpstreamModelId,
        reason: &str,
    ) {
        let now = SystemTime::now();
        let mut entries = self.entries.lock().expect("turn state mutex poisoned");
        let entry = entry_mut(&mut entries, TurnStateKey::new(account_id, model), now);
        entry.state = None;
        entry.captured_at = None;
        entry.first_applied_at = None;
        entry.expires_at = None;
        entry.source = None;
        entry.invalidated_at = Some(Utc::now());
        entry.invalidation_reason = Some(reason.to_owned());
        entry.touched_at = now;
    }

    pub(crate) fn remove_account(&self, account_id: &ProviderAccountId) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|key, _| &key.account_id != account_id);
        }
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
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        let renewal_boundary = SystemTime::now() + RENEW_BEFORE;
        entries
            .iter()
            .filter(|(_, entry)| {
                entry
                    .expires_at
                    .is_some_and(|expires_at| expires_at <= renewal_boundary)
                    || entry.invalidation_reason.is_some()
            })
            .map(|(key, _)| TurnStateProbeSubject {
                account_id: key.account_id.clone(),
                model: key.model.clone(),
            })
            .collect()
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
}

fn entry_mut(
    entries: &mut HashMap<TurnStateKey, Entry>,
    key: TurnStateKey,
    now: SystemTime,
) -> &mut Entry {
    if !entries.contains_key(&key) && entries.len() >= MAX_ENTRIES {
        let oldest = entries
            .iter()
            .min_by_key(|(_, entry)| entry.touched_at)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            entries.remove(&oldest);
        }
    }
    entries.entry(key).or_insert_with(|| Entry::empty(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> (ProviderAccountId, UpstreamModelId) {
        (
            ProviderAccountId::new("acct_turn_state_store").expect("account ID"),
            UpstreamModelId::new("gpt-5.4").expect("upstream model"),
        )
    }

    #[test]
    fn retrieving_state_does_not_mark_it_as_applied() {
        let store = TurnStateStore::new();
        let (account_id, model) = subject();
        let state = "a".repeat(TURN_STATE_BYTES);
        store.put(
            &account_id,
            &model,
            state.clone(),
            TurnStateSource::ManualProbe,
        );

        assert_eq!(
            store.state(&account_id, &model).as_deref(),
            Some(state.as_str())
        );
        assert_eq!(
            store
                .snapshot(&account_id, &model)
                .expect("turn state snapshot")
                .state_first_applied_at,
            None
        );

        assert!(store.mark_applied(&account_id, &model, &state));
        assert!(
            store
                .snapshot(&account_id, &model)
                .expect("applied turn state snapshot")
                .state_first_applied_at
                .is_some()
        );
    }

    #[test]
    fn stale_application_does_not_mark_rotated_state() {
        let store = TurnStateStore::new();
        let (account_id, model) = subject();
        let old_state = "a".repeat(TURN_STATE_BYTES);
        let new_state = "b".repeat(TURN_STATE_BYTES);
        store.put(
            &account_id,
            &model,
            old_state.clone(),
            TurnStateSource::ManualProbe,
        );
        store.put(
            &account_id,
            &model,
            new_state,
            TurnStateSource::AutomaticRenewal,
        );

        assert!(!store.mark_applied(&account_id, &model, &old_state));
        assert_eq!(
            store
                .snapshot(&account_id, &model)
                .expect("rotated turn state snapshot")
                .state_first_applied_at,
            None
        );
    }
}
