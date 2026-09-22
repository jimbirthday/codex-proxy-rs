//! 账号隔离的上游响应头续带；原始值只存在于 Provider 内存。

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use gateway_admin::model::turn_state::{
    ResponseHeaderCarryPolicy, ResponseHeaderCarryRule, ResponseHeaderCarryRuleStatus,
    ResponseHeaderCarryScope, ResponseHeaderCarrySource, ResponseHeaderCarryStatus,
    ResponseHeaderMergeMode, ResponseHeaderMissingBehavior, ResponseHeaderTransform,
    ResponseHeaderValueSelection, TurnStateRuntimeClear,
};
use gateway_core::{
    account::{CredentialRevision, OutboundProxy, ProviderAccountId},
    routing::UpstreamModelId,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

const MAX_ENTRIES: usize = 20_000;
const MAX_TOTAL_VALUE_BYTES: usize = 64 * 1024 * 1024;
const MAX_COOKIE_HEADER_BYTES: usize = 64 * 1024;

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    account_id: ProviderAccountId,
    credential_revision: CredentialRevision,
    model: Option<UpstreamModelId>,
    proxy: Option<OutboundProxy>,
    rule_id: String,
}

struct CacheEntry {
    values: Vec<CachedValue>,
    captured_at: SystemTime,
    touched_at: SystemTime,
}

struct CachedValue {
    value: HeaderValue,
    expires_at: Option<SystemTime>,
}

#[derive(Clone)]
pub(crate) struct ResponseHeaderCarryStore {
    policy: Arc<RwLock<PolicyState>>,
    revisions: Arc<RwLock<HashMap<ProviderAccountId, CredentialRevision>>>,
    entries: Arc<RwLock<HashMap<CacheKey, CacheEntry>>>,
}

struct PolicyState {
    revision: u64,
    policy: ResponseHeaderCarryPolicy,
}

/// 请求发出时冻结的规则代次。配置变更后，旧请求的迟到响应不得写入新规则缓存。
#[derive(Clone, Copy)]
pub(crate) struct ResponseHeaderCarryCaptureToken(u64);

pub(crate) struct ResponseHeaderContext<'a> {
    pub(crate) account_id: &'a ProviderAccountId,
    pub(crate) credential_revision: CredentialRevision,
    pub(crate) model: &'a UpstreamModelId,
    pub(crate) proxy: Option<&'a OutboundProxy>,
}

impl ResponseHeaderCarryStore {
    pub(crate) fn new() -> Self {
        Self {
            policy: Arc::new(RwLock::new(PolicyState {
                revision: 0,
                policy: ResponseHeaderCarryPolicy::default(),
            })),
            revisions: Arc::new(RwLock::new(HashMap::new())),
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub(crate) fn set_policy(&self, policy: ResponseHeaderCarryPolicy) {
        let previous = {
            let Ok(mut current) = self.policy.write() else {
                return;
            };
            if current.policy == policy {
                return;
            }
            let previous = std::mem::replace(&mut current.policy, policy.clone());
            current.revision = current.revision.wrapping_add(1);
            previous
        };
        let previous = previous
            .rules
            .into_iter()
            .map(|rule| (rule.id.clone(), rule))
            .collect::<HashMap<_, _>>();
        let current = policy
            .rules
            .iter()
            .map(|rule| (rule.id.as_str(), rule))
            .collect::<HashMap<_, _>>();
        if let Ok(mut entries) = self.entries.write() {
            entries.retain(|key, _| {
                let Some(rule) = current.get(key.rule_id.as_str()) else {
                    return false;
                };
                let Some(old) = previous.get(&key.rule_id) else {
                    return false;
                };
                if rule.clear_on_disable && old.enabled && !rule.enabled {
                    return false;
                }
                same_cache_contract(old, rule)
            });
        }
    }

    pub(crate) fn inject(
        &self,
        context: &ResponseHeaderContext<'_>,
        headers: &mut HeaderMap,
    ) -> ResponseHeaderCarryCaptureToken {
        let (token, policy) = match self.policy.read() {
            Ok(state) => (
                ResponseHeaderCarryCaptureToken(state.revision),
                state.policy.clone(),
            ),
            Err(_) => return ResponseHeaderCarryCaptureToken(u64::MAX),
        };
        let now = SystemTime::now();
        let Some(_revision_guard) = self.accept_revision(context) else {
            return token;
        };
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(_) => return token,
        };
        purge_stale_revisions(&mut entries, context);
        for rule in policy
            .rules
            .iter()
            .filter(|rule| rule.enabled && rule.injection_enabled && rule_matches(rule, context))
        {
            let key = cache_key(context, rule);
            let Some(entry) = entries.get_mut(&key) else {
                continue;
            };
            if entry.captured_at + Duration::from_secs(rule.ttl_seconds) <= now {
                entries.remove(&key);
                continue;
            }
            entry
                .values
                .retain(|value| value.expires_at.is_none_or(|expires_at| expires_at > now));
            if entry.values.is_empty() {
                entries.remove(&key);
                continue;
            }
            let Ok(name) = HeaderName::from_bytes(rule.target_header.as_bytes()) else {
                continue;
            };
            if rule.merge_mode == ResponseHeaderMergeMode::IfAbsent && headers.contains_key(&name) {
                continue;
            }
            if rule.merge_mode == ResponseHeaderMergeMode::Replace {
                headers.remove(&name);
            }
            if rule.transform == ResponseHeaderTransform::SetCookieToCookie {
                let mut joined = Vec::new();
                for value in &entry.values {
                    if !joined.is_empty() {
                        joined.extend_from_slice(b"; ");
                    }
                    joined.extend_from_slice(value.value.as_bytes());
                }
                if joined.len() <= MAX_COOKIE_HEADER_BYTES
                    && let Ok(mut value) = HeaderValue::from_bytes(&joined)
                {
                    value.set_sensitive(true);
                    headers.append(name, value);
                }
            } else {
                for value in &entry.values {
                    headers.append(name.clone(), value.value.clone());
                }
            }
            entry.touched_at = now;
        }
        token
    }

    pub(crate) fn capture(
        &self,
        context: &ResponseHeaderContext<'_>,
        token: ResponseHeaderCarryCaptureToken,
        source: ResponseHeaderCarrySource,
        status: u16,
        headers: &[(String, Bytes)],
    ) {
        let policy = match self.policy.read() {
            Ok(state) if state.revision == token.0 => state.policy.clone(),
            Err(_) => return,
            _ => return,
        };
        let now = SystemTime::now();
        let Some(_revision_guard) = self.accept_revision(context) else {
            return;
        };
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(_) => return,
        };
        purge_stale_revisions(&mut entries, context);
        let mut total_bytes = total_value_bytes(&entries);
        for rule in policy.rules.iter().filter(|rule| {
            rule.enabled
                && rule.capture_enabled
                && rule.sources.contains(&source)
                && rule_matches(rule, context)
        }) {
            let key = cache_key(context, rule);
            if rule.invalidation_statuses.contains(&status) {
                subtract_removed_bytes(&mut total_bytes, entries.remove(&key));
                continue;
            }
            if status < rule.capture_status_min || status > rule.capture_status_max {
                continue;
            }
            let mut raw_values = headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case(&rule.source_header))
                .filter(|(_, value)| value.len() <= rule.max_value_bytes as usize)
                .map(|(_, value)| value.as_ref())
                .take(usize::from(rule.max_values))
                .collect::<Vec<_>>();
            raw_values = match rule.value_selection {
                ResponseHeaderValueSelection::First => raw_values.into_iter().take(1).collect(),
                ResponseHeaderValueSelection::Last => {
                    raw_values.into_iter().rev().take(1).collect()
                }
                ResponseHeaderValueSelection::All => raw_values,
            };
            if raw_values.is_empty() {
                if rule.missing_behavior == ResponseHeaderMissingBehavior::Clear {
                    subtract_removed_bytes(&mut total_bytes, entries.remove(&key));
                }
                continue;
            }
            let values = if rule.transform == ResponseHeaderTransform::SetCookieToCookie {
                merge_cookie_values(entries.get(&key), &raw_values, now)
            } else {
                raw_values
                    .into_iter()
                    .filter_map(|value| HeaderValue::from_bytes(value).ok())
                    .map(|mut value| {
                        value.set_sensitive(true);
                        CachedValue {
                            value,
                            expires_at: None,
                        }
                    })
                    .collect()
            };
            if values.is_empty() {
                subtract_removed_bytes(&mut total_bytes, entries.remove(&key));
                continue;
            }
            let replacement_bytes = values.iter().map(cached_value_bytes).sum::<usize>();
            let existing_bytes = entries
                .get(&key)
                .map_or(0, |entry| entry.values.iter().map(cached_value_bytes).sum());
            while (entries.len() >= MAX_ENTRIES && !entries.contains_key(&key))
                || total_bytes
                    .saturating_sub(existing_bytes)
                    .saturating_add(replacement_bytes)
                    > MAX_TOTAL_VALUE_BYTES
            {
                if let Some(oldest) = entries
                    .iter()
                    .filter(|(candidate, _)| *candidate != &key)
                    .min_by_key(|(_, entry)| entry.touched_at)
                    .map(|(key, _)| key.clone())
                {
                    subtract_removed_bytes(&mut total_bytes, entries.remove(&oldest));
                } else {
                    break;
                }
            }
            if (entries.len() < MAX_ENTRIES || entries.contains_key(&key))
                && total_bytes
                    .saturating_sub(existing_bytes)
                    .saturating_add(replacement_bytes)
                    <= MAX_TOTAL_VALUE_BYTES
            {
                entries.insert(
                    key,
                    CacheEntry {
                        values,
                        captured_at: now,
                        touched_at: now,
                    },
                );
                total_bytes = total_bytes
                    .saturating_sub(existing_bytes)
                    .saturating_add(replacement_bytes);
            }
        }
    }

    pub(crate) fn clear(&self, command: &TurnStateRuntimeClear) {
        let Ok(mut entries) = self.entries.write() else {
            return;
        };
        entries.retain(|key, _| {
            !command
                .account_id
                .as_deref()
                .is_none_or(|account_id| key.account_id.as_str() == account_id)
                || !command.model.as_deref().is_none_or(|model| {
                    key.model
                        .as_ref()
                        .is_some_and(|value| value.as_str() == model)
                })
                || !command
                    .rule_id
                    .as_deref()
                    .is_none_or(|rule_id| key.rule_id == rule_id)
        });
    }

    pub(crate) fn remove_account(&self, account_id: &ProviderAccountId) {
        if let Ok(mut revisions) = self.revisions.write() {
            revisions.remove(account_id);
        }
        if let Ok(mut entries) = self.entries.write() {
            entries.retain(|key, _| &key.account_id != account_id);
        }
    }

    pub(crate) fn status(&self) -> ResponseHeaderCarryStatus {
        let policy = self.policy.read().map_or_else(
            |_| ResponseHeaderCarryPolicy::default(),
            |value| value.policy.clone(),
        );
        let now = SystemTime::now();
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(_) => {
                return ResponseHeaderCarryStatus {
                    total_entries: 0,
                    rules: Vec::new(),
                };
            }
        };
        let ttl_by_rule = policy
            .rules
            .iter()
            .map(|rule| (rule.id.as_str(), rule.ttl_seconds))
            .collect::<HashMap<_, _>>();
        entries.retain(|key, entry| {
            ttl_by_rule
                .get(key.rule_id.as_str())
                .is_some_and(|ttl| entry.captured_at + Duration::from_secs(*ttl) > now)
        });
        let mut rules = policy
            .rules
            .iter()
            .map(|rule| {
                let matching = entries
                    .iter()
                    .filter(|(key, _)| key.rule_id == rule.id)
                    .map(|(_, entry)| entry.captured_at)
                    .collect::<Vec<_>>();
                ResponseHeaderCarryRuleStatus {
                    rule_id: rule.id.clone(),
                    cached_entries: matching.len(),
                    last_updated_at: matching.into_iter().max().map(DateTime::<Utc>::from),
                }
            })
            .collect::<Vec<_>>();
        rules.sort_by(|left, right| left.rule_id.cmp(&right.rule_id));
        ResponseHeaderCarryStatus {
            total_entries: entries.len(),
            rules,
        }
    }

    fn accept_revision<'a>(
        &'a self,
        context: &ResponseHeaderContext<'_>,
    ) -> Option<std::sync::RwLockWriteGuard<'a, HashMap<ProviderAccountId, CredentialRevision>>>
    {
        let Ok(mut revisions) = self.revisions.write() else {
            return None;
        };
        match revisions.get(context.account_id) {
            Some(current) if *current > context.credential_revision => None,
            Some(current) if *current == context.credential_revision => Some(revisions),
            _ => {
                revisions.insert(context.account_id.clone(), context.credential_revision);
                Some(revisions)
            }
        }
    }
}

fn cache_key(context: &ResponseHeaderContext<'_>, rule: &ResponseHeaderCarryRule) -> CacheKey {
    let (model, proxy) = match rule.scope {
        ResponseHeaderCarryScope::Account => (None, None),
        ResponseHeaderCarryScope::AccountModel => (Some(context.model.clone()), None),
        ResponseHeaderCarryScope::AccountModelProxy => {
            (Some(context.model.clone()), context.proxy.cloned())
        }
    };
    CacheKey {
        account_id: context.account_id.clone(),
        credential_revision: context.credential_revision,
        model,
        proxy,
        rule_id: rule.id.clone(),
    }
}

fn purge_stale_revisions(
    entries: &mut HashMap<CacheKey, CacheEntry>,
    context: &ResponseHeaderContext<'_>,
) {
    entries.retain(|key, _| {
        &key.account_id != context.account_id
            || key.credential_revision == context.credential_revision
    });
}

fn same_cache_contract(left: &ResponseHeaderCarryRule, right: &ResponseHeaderCarryRule) -> bool {
    left.source_header
        .eq_ignore_ascii_case(&right.source_header)
        && left.sources == right.sources
        && left.transform == right.transform
        && left.value_selection == right.value_selection
        && left.scope == right.scope
        && left.account_ids == right.account_ids
        && left.models == right.models
        && left.max_value_bytes == right.max_value_bytes
        && left.max_values == right.max_values
}

fn rule_matches(rule: &ResponseHeaderCarryRule, context: &ResponseHeaderContext<'_>) -> bool {
    (rule.account_ids.is_empty()
        || rule
            .account_ids
            .iter()
            .any(|value| value == context.account_id.as_str()))
        && (rule.models.is_empty()
            || rule
                .models
                .iter()
                .any(|value| value == context.model.as_str()))
}

fn merge_cookie_values(
    current: Option<&CacheEntry>,
    headers: &[&[u8]],
    now: SystemTime,
) -> Vec<CachedValue> {
    let mut values = BTreeMap::<String, CachedValue>::new();
    if let Some(current) = current {
        for value in &current.values {
            if value.expires_at.is_some_and(|expires_at| expires_at <= now) {
                continue;
            }
            let Some((name, _)) = std::str::from_utf8(value.value.as_bytes())
                .ok()
                .and_then(|value| value.split_once('='))
            else {
                continue;
            };
            values.insert(
                name.to_owned(),
                CachedValue {
                    value: value.value.clone(),
                    expires_at: value.expires_at,
                },
            );
        }
    }
    for header in headers {
        let Some(cookie) = std::str::from_utf8(header)
            .ok()
            .and_then(|value| cookie::Cookie::parse(value.to_owned()).ok())
        else {
            continue;
        };
        let name = cookie.name().to_owned();
        let max_age = cookie.max_age().map(|value| value.whole_seconds());
        let expires_timestamp = cookie
            .expires_datetime()
            .map(|value| value.unix_timestamp());
        let delete = cookie.value().is_empty()
            || max_age.is_some_and(|seconds| seconds <= 0)
            || expires_timestamp
                .is_some_and(|timestamp| timestamp <= chrono::Utc::now().timestamp());
        if delete {
            values.remove(&name);
            continue;
        }
        let expires_at = max_age
            .and_then(|seconds| u64::try_from(seconds).ok())
            .and_then(|seconds| now.checked_add(Duration::from_secs(seconds)))
            .or_else(|| {
                expires_timestamp
                    .and_then(|timestamp| u64::try_from(timestamp).ok())
                    .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
            });
        let Ok(mut value) = HeaderValue::from_str(&format!("{}={}", cookie.name(), cookie.value()))
        else {
            continue;
        };
        value.set_sensitive(true);
        values.insert(name, CachedValue { value, expires_at });
    }
    values.into_values().collect()
}

fn cached_value_bytes(value: &CachedValue) -> usize {
    value.value.as_bytes().len()
}

fn total_value_bytes(entries: &HashMap<CacheKey, CacheEntry>) -> usize {
    entries
        .values()
        .flat_map(|entry| &entry.values)
        .map(cached_value_bytes)
        .sum()
}

fn subtract_removed_bytes(total: &mut usize, removed: Option<CacheEntry>) {
    if let Some(removed) = removed {
        *total = total.saturating_sub(removed.values.iter().map(cached_value_bytes).sum());
    }
}
