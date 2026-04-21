use std::collections::{HashMap, HashSet, VecDeque};

use chrono::Utc;
use colored::Colorize;
use moka::sync::Cache;
use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
use serde::Serialize;
use snafu::{GenerateImplicitData, Location};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::{
    claude_code_state::probe::probe_cookie,
    config::{AccountSlot, ClewdrCookie, InvalidAccountSlot, Reason, UsageBreakdown},
    db::accounts::{
        AccountSummary, active_reset_time, batch_upsert_runtime_states, load_all_accounts,
        set_account_disabled, set_accounts_active, summarize_accounts, upsert_account_oauth,
    },
    error::ClewdrError,
    state::AdminEvent,
    stealth,
};

const INTERVAL: u64 = 300;
const FLUSH_INTERVAL: u64 = 15;
const SESSION_WINDOW_SECS: i64 = 5 * 60 * 60; // 5h
const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 60 * 60; // 7d

/// Build a unique placeholder cookie string for an oauth-only account so its
/// `AccountSlot` stays distinguishable in HashSet/moka keyed on the cookie value.
/// The format satisfies `ClewdrCookie`'s regex (`sk-ant-sid\d{2}-[A-Za-z0-9_-]{86,120}-[A-Za-z0-9_-]{6}AA`).
fn oauth_placeholder_cookie(account_id: i64) -> String {
    format!("sk-ant-sid99-o{:0>85}-pool00AA", account_id)
}

/// Returns true if `cookie` was minted by `oauth_placeholder_cookie`. Used to
/// keep the pool's cookie-style probe paths (`spawn_probes_for_unprobed`,
/// `spawn_probe_all`, `spawn_probe_accounts`) from running `probe_cookie`
/// against an oauth-only slot — the placeholder is not a real session cookie,
/// so a cookie probe would fail and drive a healthy oauth account to invalid.
/// Oauth accounts have a separate probe path (`probe_oauth_account`) invoked
/// directly from the admin API.
pub(crate) fn is_oauth_placeholder_slot(cookie: &AccountSlot) -> bool {
    let raw = cookie.cookie.to_string();
    // `ClewdrCookie::Display` produces `sessionKey=<inner>`; match the inner pattern.
    raw.contains("sk-ant-sid99-o") && raw.ends_with("-pool00AA")
}

#[derive(Debug, Serialize, Clone)]
pub struct AccountPoolStatus {
    pub valid: Vec<AccountSlot>,
    pub exhausted: Vec<AccountSlot>,
    pub invalid: Vec<InvalidAccountSlot>,
}

#[derive(Debug)]
enum AccountPoolMessage {
    Return(AccountSlot, Option<Reason>),
    Submit(AccountSlot),
    CheckReset,
    Request(
        Option<u64>,
        Vec<i64>,
        RpcReplyPort<Result<AccountSlot, ClewdrError>>,
    ),
    GetStatus(RpcReplyPort<AccountPoolStatus>),
    ReloadFromDb,
    ProbeAll(RpcReplyPort<Vec<i64>>),
    ProbeAccounts(
        Vec<i64>,
        broadcast::Sender<AdminEvent>,
        RpcReplyPort<Vec<i64>>,
    ),
    BeginProbe(i64, RpcReplyPort<bool>),
    FlushDirty,
    SetHandle(AccountPoolHandle),
    ReleaseSlot(i64),
    GetProbingIds(RpcReplyPort<Vec<i64>>),
    ClearProbing(i64),
    SetProbeError(i64, String),
    ClearProbeError(i64),
    GetProbeErrors(RpcReplyPort<HashMap<i64, String>>),
}

#[derive(Debug)]
struct AccountPoolState {
    valid: VecDeque<AccountSlot>,
    exhausted: HashSet<AccountSlot>,
    invalid: HashSet<InvalidAccountSlot>,
    moka: Cache<u64, AccountSlot>,
    db: SqlitePool,
    event_tx: broadcast::Sender<AdminEvent>,
    dirty: HashSet<i64>,
    handle: Option<AccountPoolHandle>,
    /// Per-account inflight tracking: account_id → (current_inflight, max_slots)
    inflight: HashMap<i64, (u32, u32)>,
    probing: HashSet<i64>,
    reactivated: HashSet<i64>,
    /// Last probe error per account (transient errors only, cleared on success)
    probe_errors: HashMap<i64, String>,
    /// Account IDs marked with `drain_first = true`. These are preferred
    /// during dispatch until all of them have no available inflight slot.
    drain_first_ids: HashSet<i64>,
}

struct AccountPoolActor;

impl AccountPoolActor {
    fn emit_accounts_refresh(state: &AccountPoolState) {
        let _ = state.event_tx.send(AdminEvent::accounts_refresh());
    }

    fn mark_dirty(state: &mut AccountPoolState, account_id: Option<i64>) {
        if let Some(id) = account_id {
            state.dirty.insert(id);
        }
    }

    fn mark_all_dirty(state: &mut AccountPoolState) {
        for cs in state.valid.iter().chain(state.exhausted.iter()) {
            if let Some(id) = cs.account_id {
                state.dirty.insert(id);
            }
        }
        for uc in &state.invalid {
            if let Some(id) = uc.account_id {
                state.dirty.insert(id);
            }
        }
    }

    fn log(state: &AccountPoolState) {
        info!(
            "Valid: {}, Exhausted: {}, Invalid: {}",
            state.valid.len().to_string().green(),
            state.exhausted.len().to_string().yellow(),
            state.invalid.len().to_string().red(),
        );
    }

    fn log_account_summary(summary: AccountSummary) {
        info!(
            "Valid: {}, Exhausted: {}, Invalid: {}",
            summary.pool.valid.to_string().green(),
            summary.pool.exhausted.to_string().yellow(),
            summary.pool.invalid.to_string().red(),
        );
    }

    fn reset(state: &mut AccountPoolState) {
        let mut reset_cookies = Vec::new();
        state.exhausted.retain(|cookie| {
            let reset_cookie = cookie.clone().reset();
            if reset_cookie.reset_time.is_none() {
                reset_cookies.push(reset_cookie);
                false
            } else {
                true
            }
        });
        if reset_cookies.is_empty() {
            return;
        }
        for c in reset_cookies {
            Self::mark_dirty(state, c.account_id);
            state.valid.push_back(c);
        }
        Self::log(state);
    }

    fn refresh_usage_windows(state: &mut AccountPoolState) -> bool {
        fn reset_if_due(
            has_reset: Option<bool>,
            resets_at: &mut Option<i64>,
            usage: &mut UsageBreakdown,
            utilization: &mut Option<f64>,
            window_secs: i64,
            now: i64,
        ) -> bool {
            if has_reset == Some(true) && resets_at.map(|ts| now >= ts).unwrap_or(false) {
                *usage = UsageBreakdown::default();
                *utilization = Some(0.0);
                *resets_at = Some(now + window_secs);
                return true;
            }
            false
        }

        let now = Utc::now().timestamp();
        let mut changed = false;

        let apply_resets = |cookie: &mut AccountSlot| {
            let mut cookie_changed = reset_if_due(
                cookie.session_has_reset,
                &mut cookie.session_resets_at,
                &mut cookie.session_usage,
                &mut cookie.session_utilization,
                SESSION_WINDOW_SECS,
                now,
            );
            cookie_changed |= reset_if_due(
                cookie.weekly_has_reset,
                &mut cookie.weekly_resets_at,
                &mut cookie.weekly_usage,
                &mut cookie.weekly_utilization,
                WEEKLY_WINDOW_SECS,
                now,
            );
            cookie_changed |= reset_if_due(
                cookie.weekly_sonnet_has_reset,
                &mut cookie.weekly_sonnet_resets_at,
                &mut cookie.weekly_sonnet_usage,
                &mut cookie.weekly_sonnet_utilization,
                WEEKLY_WINDOW_SECS,
                now,
            );
            cookie_changed |= reset_if_due(
                cookie.weekly_opus_has_reset,
                &mut cookie.weekly_opus_resets_at,
                &mut cookie.weekly_opus_usage,
                &mut cookie.weekly_opus_utilization,
                WEEKLY_WINDOW_SECS,
                now,
            );
            cookie_changed
        };

        let mut dirty_from_valid = Vec::new();
        for cookie in state.valid.iter_mut() {
            if apply_resets(cookie) {
                changed = true;
                if let Some(id) = cookie.account_id {
                    dirty_from_valid.push(id);
                }
            }
        }
        for id in dirty_from_valid {
            state.dirty.insert(id);
        }

        if !state.exhausted.is_empty() {
            let mut dirty_from_exhausted = Vec::new();
            let mut new_exhausted = HashSet::with_capacity(state.exhausted.len());
            for mut cookie in state.exhausted.drain() {
                if apply_resets(&mut cookie) {
                    changed = true;
                    if let Some(id) = cookie.account_id {
                        dirty_from_exhausted.push(id);
                    }
                }
                new_exhausted.insert(cookie);
            }
            state.exhausted = new_exhausted;
            for id in dirty_from_exhausted {
                state.dirty.insert(id);
            }
        }

        changed
    }

    fn dispatch(
        &self,
        state: &mut AccountPoolState,
        hash: Option<u64>,
        bound: &[i64],
    ) -> Result<AccountSlot, ClewdrError> {
        use std::hash::{DefaultHasher, Hash, Hasher};
        Self::reset(state);

        let cache_key = hash.map(|h| {
            if bound.is_empty() {
                h
            } else {
                let mut hasher = DefaultHasher::new();
                h.hash(&mut hasher);
                bound.hash(&mut hasher);
                hasher.finish()
            }
        });

        let is_allowed = |c: &AccountSlot, inflight: &HashMap<i64, (u32, u32)>| -> bool {
            let bound_ok = bound.is_empty() || c.account_id.is_some_and(|id| bound.contains(&id));
            let slot_ok = c.account_id.map_or(true, |id| {
                inflight.get(&id).map_or(true, |(cur, max)| cur < max)
            });
            bound_ok && slot_ok
        };

        // Phase 1: prefer accounts flagged `drain_first` that are still allowed
        // and have available inflight slots. This overrides moka cache affinity
        // on purpose — the intent of `drain_first` is to concentrate usage on
        // these accounts until they are saturated or cooling down, after which
        // we fall through to the normal round-robin / cached selection.
        if !state.drain_first_ids.is_empty()
            && let Some(idx) = state.valid.iter().position(|c| {
                is_allowed(c, &state.inflight)
                    && c.account_id
                        .is_some_and(|id| state.drain_first_ids.contains(&id))
            })
        {
            let cookie = state.valid.remove(idx).unwrap();
            if let Some(aid) = cookie.account_id {
                if let Some((cur, _)) = state.inflight.get_mut(&aid) {
                    *cur += 1;
                }
            }
            state.valid.push_back(cookie.clone());
            if let Some(key) = cache_key {
                state.moka.insert(key, cookie.clone());
            }
            return Ok(cookie);
        }

        if let Some(key) = cache_key
            && let Some(cached) = state.moka.get(&key)
            && let Some(cookie) = state
                .valid
                .iter()
                .find(|c| *c == &cached && is_allowed(c, &state.inflight))
        {
            let cookie = cookie.clone();
            if let Some(aid) = cookie.account_id {
                if let Some((cur, _)) = state.inflight.get_mut(&aid) {
                    *cur += 1;
                }
            }
            state.moka.insert(key, cookie.clone());
            return Ok(cookie);
        }

        let idx = match state
            .valid
            .iter()
            .position(|c| is_allowed(c, &state.inflight))
        {
            Some(idx) => idx,
            None => {
                // Determine whether this is a temporary (cooling) or permanent (invalid/empty) failure.
                let has_relevant_valid = state.valid.iter().any(|c| {
                    bound.is_empty() || c.account_id.is_some_and(|id| bound.contains(&id))
                });
                let has_relevant_exhausted = state.exhausted.iter().any(|c| {
                    bound.is_empty() || c.account_id.is_some_and(|id| bound.contains(&id))
                });
                // Valid accounts exist but all slots are full, or some are in cooldown → temporary
                return Err(if has_relevant_valid || has_relevant_exhausted {
                    ClewdrError::UpstreamCoolingDown
                } else {
                    ClewdrError::NoValidUpstreamAccounts
                });
            }
        };

        let cookie = state.valid.remove(idx).unwrap();
        if let Some(aid) = cookie.account_id {
            if let Some((cur, _)) = state.inflight.get_mut(&aid) {
                *cur += 1;
            }
        }
        state.valid.push_back(cookie.clone());
        if let Some(key) = cache_key {
            state.moka.insert(key, cookie.clone());
        }
        Ok(cookie)
    }

    fn collect(state: &mut AccountPoolState, cookie: AccountSlot, reason: Option<Reason>) -> bool {
        let aid = cookie.account_id;

        let removed_probe = aid.is_some_and(|id| state.probing.remove(&id));

        // Remove from whichever set the cookie currently lives in
        let was_valid = state
            .valid
            .iter()
            .position(|c| *c == cookie)
            .map(|i| state.valid.remove(i).unwrap());
        let was_exhausted = state.exhausted.take(&cookie);
        let tmp = InvalidAccountSlot::new(cookie.cookie.clone(), Reason::Null);
        let was_invalid = state.invalid.take(&tmp);

        if was_valid.is_none() && was_exhausted.is_none() && was_invalid.is_none() {
            return removed_probe;
        }

        let changed_set = match &reason {
            None => {
                if cookie.reset_time.is_some() {
                    let was_ex = was_exhausted.is_some();
                    state.exhausted.insert(cookie);
                    !was_ex
                } else {
                    let was_val = was_valid.is_some();
                    state.valid.push_back(cookie);
                    !was_val
                }
            }
            Some(Reason::TooManyRequest(i) | Reason::Restricted(i)) => {
                let mut cookie = cookie;
                cookie.reset_time = Some(*i);
                cookie.reset_window_usage();
                let was_ex = was_exhausted.is_some();
                state.exhausted.insert(cookie);
                !was_ex
            }
            Some(reason) => {
                let mut cookie = cookie;
                cookie.reset_window_usage();
                let was_inv = was_invalid.is_some();
                state.invalid.insert(InvalidAccountSlot::with_account_id(
                    cookie.cookie.clone(),
                    reason.clone(),
                    aid,
                ));
                !was_inv
            }
        };

        // Track invalid → valid/exhausted for DB status reactivation
        let moved_out_of_invalid = was_invalid.is_some()
            && matches!(
                &reason,
                None | Some(Reason::TooManyRequest(_) | Reason::Restricted(_))
            );
        if moved_out_of_invalid {
            if let Some(id) = aid {
                state.reactivated.insert(id);
            }
        }

        Self::mark_dirty(state, aid);
        if changed_set {
            Self::log(state);
        }
        removed_probe
    }

    fn accept(state: &mut AccountPoolState, cookie: AccountSlot) {
        if state.valid.contains(&cookie)
            || state.exhausted.contains(&cookie)
            || state.invalid.iter().any(|c| *c == cookie)
        {
            warn!("Cookie already exists");
            return;
        }
        let needs_probe = cookie.email.is_none() || cookie.account_type.is_none();
        let aid = cookie.account_id;
        state.valid.push_back(cookie.clone());
        Self::mark_dirty(state, aid);
        Self::log(state);

        if needs_probe {
            Self::spawn_probe_guarded(state, &cookie, None);
        }
    }

    fn spawn_probe_guarded(
        state: &mut AccountPoolState,
        cookie: &AccountSlot,
        log_sink: Option<broadcast::Sender<AdminEvent>>,
    ) {
        let Some(account_id) = cookie.account_id else {
            return;
        };
        if state.probing.contains(&account_id) {
            return;
        }
        let Some(ref handle) = state.handle else {
            return;
        };
        state.probing.insert(account_id);
        state.probe_errors.remove(&account_id);
        Self::emit_accounts_refresh(state);
        let handle = handle.clone();
        let cookie = cookie.clone();
        let db = state.db.clone();
        let profile = stealth::global_profile().clone();
        tokio::spawn(async move {
            probe_cookie(account_id, cookie, handle, profile, db, log_sink).await;
        });
    }

    fn spawn_probes_for_unprobed(state: &mut AccountPoolState) {
        let unprobed: Vec<AccountSlot> = state
            .valid
            .iter()
            .filter(|c| !is_oauth_placeholder_slot(c))
            .filter(|c| c.email.is_none() || c.account_type.is_none())
            .cloned()
            .collect();
        for cookie in &unprobed {
            Self::spawn_probe_guarded(state, cookie, None);
        }
    }

    fn spawn_probe_all(state: &mut AccountPoolState) {
        let cookies: Vec<AccountSlot> = state
            .valid
            .iter()
            .cloned()
            .chain(state.exhausted.iter().cloned())
            .filter(|c| !is_oauth_placeholder_slot(c))
            .collect();
        for cookie in &cookies {
            Self::spawn_probe_guarded(state, cookie, None);
        }

        let invalid_cookies: Vec<(ClewdrCookie, Option<i64>)> = state
            .invalid
            .iter()
            .map(|uc| (uc.cookie.clone(), uc.account_id))
            .collect();
        for (cookie_blob, account_id) in invalid_cookies {
            if let Ok(mut cs) = AccountSlot::new(&cookie_blob.to_string(), None) {
                cs.account_id = account_id;
                if is_oauth_placeholder_slot(&cs) {
                    continue;
                }
                Self::spawn_probe_guarded(state, &cs, None);
            }
        }
    }

    fn spawn_probe_accounts(
        state: &mut AccountPoolState,
        account_ids: &[i64],
        log_sink: Option<broadcast::Sender<AdminEvent>>,
    ) {
        let wanted: HashSet<i64> = account_ids.iter().copied().collect();
        if wanted.is_empty() {
            return;
        }

        let cookies: Vec<AccountSlot> = state
            .valid
            .iter()
            .cloned()
            .chain(state.exhausted.iter().cloned())
            .filter(|cookie| cookie.account_id.is_some_and(|id| wanted.contains(&id)))
            .filter(|cookie| !is_oauth_placeholder_slot(cookie))
            .collect();
        for cookie in &cookies {
            Self::spawn_probe_guarded(state, cookie, log_sink.clone());
        }

        let invalid_cookies: Vec<(ClewdrCookie, Option<i64>)> = state
            .invalid
            .iter()
            .filter(|uc| uc.account_id.is_some_and(|id| wanted.contains(&id)))
            .map(|uc| (uc.cookie.clone(), uc.account_id))
            .collect();
        for (cookie_blob, account_id) in invalid_cookies {
            if let Ok(mut cs) = AccountSlot::new(&cookie_blob.to_string(), None) {
                cs.account_id = account_id;
                if is_oauth_placeholder_slot(&cs) {
                    continue;
                }
                Self::spawn_probe_guarded(state, &cs, log_sink.clone());
            }
        }
    }

    fn report(state: &AccountPoolState) -> AccountPoolStatus {
        AccountPoolStatus {
            valid: state.valid.clone().into(),
            exhausted: state.exhausted.iter().cloned().collect(),
            invalid: state.invalid.iter().cloned().collect(),
        }
    }

    fn apply_in_memory_runtime(dst: &mut AccountSlot, mem: AccountSlot, preserve_token: bool) {
        if preserve_token {
            dst.token = mem.token;
        }
        dst.reset_time = mem.reset_time;
        dst.session_usage = mem.session_usage;
        dst.weekly_usage = mem.weekly_usage;
        dst.weekly_sonnet_usage = mem.weekly_sonnet_usage;
        dst.weekly_opus_usage = mem.weekly_opus_usage;
        dst.lifetime_usage = mem.lifetime_usage;
        dst.session_resets_at = mem.session_resets_at;
        dst.weekly_resets_at = mem.weekly_resets_at;
        dst.weekly_sonnet_resets_at = mem.weekly_sonnet_resets_at;
        dst.weekly_opus_resets_at = mem.weekly_opus_resets_at;
        dst.resets_last_checked_at = mem.resets_last_checked_at;
        dst.session_has_reset = mem.session_has_reset;
        dst.weekly_has_reset = mem.weekly_has_reset;
        dst.weekly_sonnet_has_reset = mem.weekly_sonnet_has_reset;
        dst.weekly_opus_has_reset = mem.weekly_opus_has_reset;
        dst.supports_claude_1m_sonnet = mem.supports_claude_1m_sonnet;
        dst.supports_claude_1m_opus = mem.supports_claude_1m_opus;
        dst.count_tokens_allowed = mem.count_tokens_allowed;
        dst.session_utilization = mem.session_utilization;
        dst.weekly_utilization = mem.weekly_utilization;
        dst.weekly_sonnet_utilization = mem.weekly_sonnet_utilization;
        dst.weekly_opus_utilization = mem.weekly_opus_utilization;
        // Prefer memory email/account_type if DB is null but memory has it.
        if dst.email.is_none() {
            dst.email = mem.email;
        }
        if dst.account_type.is_none() {
            dst.account_type = mem.account_type;
        }
    }

    async fn do_flush(state: &mut AccountPoolState) {
        if state.dirty.is_empty() {
            return;
        }
        let dirty_ids: HashSet<i64> = std::mem::take(&mut state.dirty);

        let mut params = Vec::new();
        let mut oauth_updates = Vec::new();
        for cs in state.valid.iter().chain(state.exhausted.iter()) {
            if let Some(id) = cs.account_id {
                if dirty_ids.contains(&id) {
                    params.push((id, cs.to_runtime_params()));
                    oauth_updates.push((id, cs.token.clone()));
                }
            }
        }

        let mut disabled = Vec::new();
        for uc in &state.invalid {
            if let Some(id) = uc.account_id {
                if dirty_ids.contains(&id) {
                    disabled.push((id, uc.reason.to_db_string()));
                }
            }
        }

        // Await directly — 1-3 accounts, <1ms. On failure, re-insert dirty IDs.
        if let Err(e) = batch_upsert_runtime_states(&state.db, &params).await {
            warn!("Failed to flush runtime states: {e}");
            for (id, _) in &params {
                state.dirty.insert(*id);
            }
        }
        for (id, token) in &oauth_updates {
            if let Err(e) = upsert_account_oauth(&state.db, *id, token.as_ref(), None).await {
                warn!("Failed to flush OAuth token for account {id}: {e}");
                state.dirty.insert(*id);
            }
        }

        if !state.reactivated.is_empty() {
            let ids: Vec<i64> = state.reactivated.drain().collect();
            if let Err(e) = set_accounts_active(&state.db, &ids).await {
                warn!("Failed to reactivate accounts: {e}");
            }
        }
        for (id, reason) in &disabled {
            if let Err(e) = set_account_disabled(&state.db, *id, reason).await {
                warn!("Failed to set account {id} disabled: {e}");
                state.dirty.insert(*id);
            }
        }
    }

    async fn do_reload(state: &mut AccountPoolState) {
        // Flush pending dirty state before reload to avoid losing in-memory changes
        Self::do_flush(state).await;

        let accounts = match load_all_accounts(&state.db).await {
            Ok(a) => a,
            Err(e) => {
                error!("Failed to load accounts from DB: {e}");
                return;
            }
        };

        // Index current in-memory state by account_id
        let mut mem_cookies: HashMap<i64, AccountSlot> = HashMap::new();
        for cs in state.valid.drain(..) {
            if let Some(id) = cs.account_id {
                mem_cookies.insert(id, cs);
            }
        }
        for cs in state.exhausted.drain() {
            if let Some(id) = cs.account_id {
                mem_cookies.insert(id, cs);
            }
        }
        // Drain invalid set — will be rebuilt from DB
        state.invalid.clear();

        let mut replaced_ids = Vec::new();

        // Rebuild from DB
        for row in &accounts {
            if matches!(row.status.as_str(), "disabled" | "auth_error") {
                let cookie_for_invalid = match row.cookie_blob.as_deref() {
                    Some(cookie_str) => AccountSlot::new(cookie_str, None).ok().map(|cs| cs.cookie),
                    None if row.oauth_token.is_some() => {
                        AccountSlot::new(&oauth_placeholder_cookie(row.id), None)
                            .ok()
                            .map(|cs| cs.cookie)
                    }
                    None => None,
                };
                if let Some(cookie) = cookie_for_invalid {
                    let reason = row
                        .invalid_reason
                        .as_deref()
                        .map(Reason::from_db_string)
                        .unwrap_or(Reason::Null);
                    state.invalid.insert(InvalidAccountSlot::with_account_id(
                        cookie,
                        reason,
                        Some(row.id),
                    ));
                }
                continue;
            }

            let cs_result = match row.cookie_blob.as_deref() {
                Some(cookie_str) => AccountSlot::new(cookie_str, None),
                None if row.oauth_token.is_some() => {
                    // OAuth-only account: synthesize a per-account placeholder cookie so the
                    // slot is still hashable/equal-distinct in the pool's HashSet and moka cache.
                    // The real credential is in `row.oauth_token` and is attached below.
                    AccountSlot::new(&oauth_placeholder_cookie(row.id), None)
                }
                None => continue,
            };
            let mut cs = match cs_result {
                Ok(cs) => cs,
                Err(e) => {
                    warn!("Invalid cookie for account '{}': {e}", row.name);
                    continue;
                }
            };
            cs.account_id = Some(row.id);
            cs.proxy_url = row.proxy_url.clone();
            cs.email = row.email.clone();
            cs.account_type = row.account_type.clone();
            if let Some(token) = row.oauth_token.clone() {
                cs.token = Some(token);
            }

            // Merge: if memory has same account_id with same cookie, preserve runtime
            if let Some(mem) = mem_cookies.remove(&row.id) {
                if mem.cookie == cs.cookie {
                    // Same credential — preserve runtime state from memory.
                    // OAuth credentials are replaced out-of-band by reconnect/edit flows; when
                    // DB has a token, it is the source of truth even if the placeholder cookie is
                    // deterministic and therefore matches the in-memory slot.
                    Self::apply_in_memory_runtime(&mut cs, mem, row.oauth_token.is_none());
                    cs.proxy_url = row.proxy_url.clone();
                }
                // Cookie changed = credential replacement → use fresh defaults from new()
                else {
                    replaced_ids.push(row.id);
                }
            } else if let Some(ref runtime) = row.runtime {
                let params = runtime.to_params();
                cs.apply_runtime_state(&params);
                let normalized_reset = active_reset_time(row);
                if cs.reset_time != normalized_reset {
                    cs.reset_time = normalized_reset;
                    Self::mark_dirty(state, cs.account_id);
                }
            }

            if cs.reset_time.is_some() {
                state.exhausted.insert(cs);
            } else {
                state.valid.push_back(cs);
            }
        }

        // Accounts not in DB anymore → already removed by drain + not re-inserted
        // (mem_cookies remaining entries are deleted accounts)

        // Clear moka cache since cookie set changed
        state.moka.invalidate_all();

        // Rebuild inflight map: preserve current counts, update max_slots from DB
        let mut new_inflight = HashMap::new();
        for row in &accounts {
            if row.cookie_blob.is_none() && row.oauth_token.is_none() {
                continue;
            }
            let current = state.inflight.get(&row.id).map_or(0, |(cur, _)| *cur);
            new_inflight.insert(row.id, (current, row.max_slots as u32));
        }
        state.inflight = new_inflight;

        // Rebuild the drain_first index from DB.
        state.drain_first_ids = accounts
            .iter()
            .filter(|r| r.drain_first)
            .map(|r| r.id)
            .collect();

        // Clean stale probing IDs (deleted accounts + cookie-replaced accounts)
        let current_ids: HashSet<i64> = accounts.iter().map(|r| r.id).collect();
        state.probing.retain(|id| current_ids.contains(id));
        for id in &replaced_ids {
            state.probing.remove(id);
        }

        Self::log_account_summary(summarize_accounts(&accounts));

        // Spawn probes for unprobed cookies
        Self::spawn_probes_for_unprobed(state);
        Self::emit_accounts_refresh(state);
    }
}

impl Actor for AccountPoolActor {
    type Msg = AccountPoolMessage;
    type State = AccountPoolState;
    type Arguments = (SqlitePool, broadcast::Sender<AdminEvent>);

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let (db, event_tx) = args;
        let moka = Cache::builder()
            .max_capacity(1000)
            .time_to_idle(std::time::Duration::from_secs(60 * 60))
            .build();

        let mut state = AccountPoolState {
            valid: VecDeque::new(),
            exhausted: HashSet::new(),
            invalid: HashSet::new(),
            moka,
            db,
            event_tx,
            dirty: HashSet::new(),
            handle: None,
            inflight: HashMap::new(),
            probing: HashSet::new(),
            reactivated: HashSet::new(),
            probe_errors: HashMap::new(),
            drain_first_ids: HashSet::new(),
        };

        // Load accounts from DB
        Self::do_reload(&mut state).await;

        Ok(state)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            AccountPoolMessage::Return(cookie, reason) => {
                let completed_probe = Self::collect(state, cookie, reason);
                if completed_probe {
                    Self::do_flush(state).await;
                    Self::emit_accounts_refresh(state);
                }
            }
            AccountPoolMessage::Submit(cookie) => {
                Self::accept(state, cookie);
            }
            AccountPoolMessage::CheckReset => {
                Self::refresh_usage_windows(state);
                Self::reset(state);
            }
            AccountPoolMessage::Request(cache_hash, bound, reply_port) => {
                let result = self.dispatch(state, cache_hash, &bound);
                reply_port.send(result)?;
            }
            AccountPoolMessage::GetStatus(reply_port) => {
                Self::refresh_usage_windows(state);
                let status_info = Self::report(state);
                reply_port.send(status_info)?;
            }

            AccountPoolMessage::ReloadFromDb => {
                Self::do_reload(state).await;
            }
            AccountPoolMessage::ProbeAll(reply_port) => {
                Self::spawn_probe_all(state);
                reply_port.send(state.probing.iter().copied().collect())?;
            }
            AccountPoolMessage::ProbeAccounts(account_ids, event_tx, reply_port) => {
                Self::spawn_probe_accounts(state, &account_ids, Some(event_tx));
                let probing: Vec<i64> = account_ids
                    .into_iter()
                    .filter(|id| state.probing.contains(id))
                    .collect();
                reply_port.send(probing)?;
            }
            AccountPoolMessage::BeginProbe(account_id, reply_port) => {
                let inserted = state.probing.insert(account_id);
                if inserted {
                    state.probe_errors.remove(&account_id);
                    Self::emit_accounts_refresh(state);
                }
                reply_port.send(inserted)?;
            }
            AccountPoolMessage::FlushDirty => {
                Self::do_flush(state).await;
            }
            AccountPoolMessage::SetHandle(handle) => {
                state.handle = Some(handle);
                // Backfill probes missed during pre_start (handle was None then)
                Self::spawn_probes_for_unprobed(state);
            }
            AccountPoolMessage::ReleaseSlot(account_id) => {
                if let Some((cur, _)) = state.inflight.get_mut(&account_id) {
                    *cur = cur.saturating_sub(1);
                }
            }
            AccountPoolMessage::GetProbingIds(reply_port) => {
                reply_port.send(state.probing.iter().copied().collect())?;
            }
            AccountPoolMessage::ClearProbing(account_id) => {
                if state.probing.remove(&account_id) {
                    Self::emit_accounts_refresh(state);
                }
            }
            AccountPoolMessage::SetProbeError(account_id, msg) => {
                state.probe_errors.insert(account_id, msg);
                Self::emit_accounts_refresh(state);
            }
            AccountPoolMessage::ClearProbeError(account_id) => {
                if state.probe_errors.remove(&account_id).is_some() {
                    Self::emit_accounts_refresh(state);
                }
            }
            AccountPoolMessage::GetProbeErrors(reply_port) => {
                reply_port.send(state.probe_errors.clone())?;
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // Flush all accounts on shutdown
        Self::mark_all_dirty(state);
        // Do synchronous flush (await directly in post_stop)
        let dirty_ids: HashSet<i64> = std::mem::take(&mut state.dirty);
        let mut params = Vec::new();
        for cs in state.valid.iter().chain(state.exhausted.iter()) {
            if let Some(id) = cs.account_id {
                if dirty_ids.contains(&id) {
                    params.push((id, cs.to_runtime_params()));
                }
            }
        }
        if let Err(e) = batch_upsert_runtime_states(&state.db, &params).await {
            error!("Failed to flush runtime states on shutdown: {e}");
        }
        for uc in &state.invalid {
            if let Some(id) = uc.account_id {
                if dirty_ids.contains(&id) {
                    if let Err(e) =
                        set_account_disabled(&state.db, id, &uc.reason.to_db_string()).await
                    {
                        error!("Failed to set account {id} disabled on shutdown: {e}");
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct AccountPoolHandle {
    actor_ref: ActorRef<AccountPoolMessage>,
}

impl std::fmt::Debug for AccountPoolHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountPoolHandle").finish()
    }
}

impl AccountPoolHandle {
    pub async fn start(
        db: SqlitePool,
        event_tx: broadcast::Sender<AdminEvent>,
    ) -> Result<Self, ractor::SpawnErr> {
        let (actor_ref, _join_handle) =
            Actor::spawn(None, AccountPoolActor, (db, event_tx)).await?;

        let handle = Self {
            actor_ref: actor_ref.clone(),
        };

        // Send the handle to the actor so it can spawn probe tasks
        let _ = ractor::cast!(actor_ref, AccountPoolMessage::SetHandle(handle.clone()));

        handle.spawn_timeout_checker().await;
        handle.spawn_flush_timer().await;

        Ok(handle)
    }

    async fn spawn_timeout_checker(&self) {
        let actor_ref = self.actor_ref.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(INTERVAL));
            loop {
                interval.tick().await;
                if ractor::cast!(actor_ref, AccountPoolMessage::CheckReset).is_err() {
                    break;
                }
            }
        });
    }

    async fn spawn_flush_timer(&self) {
        let actor_ref = self.actor_ref.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(FLUSH_INTERVAL));
            loop {
                interval.tick().await;
                if ractor::cast!(actor_ref, AccountPoolMessage::FlushDirty).is_err() {
                    break;
                }
            }
        });
    }

    pub async fn request(
        &self,
        cache_hash: Option<u64>,
        bound_account_ids: &[i64],
    ) -> Result<AccountSlot, ClewdrError> {
        ractor::call!(
            self.actor_ref,
            AccountPoolMessage::Request,
            cache_hash,
            bound_account_ids.to_vec()
        )
        .map_err(|e| ClewdrError::RactorError {
            loc: Location::generate(),
            msg: format!("Failed to communicate with AccountPoolActor for request operation: {e}"),
        })?
    }

    pub async fn release(
        &self,
        cookie: AccountSlot,
        reason: Option<Reason>,
    ) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, AccountPoolMessage::Return(cookie, reason)).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with AccountPoolActor for return operation: {e}"
                ),
            }
        })
    }

    pub async fn submit(&self, cookie: AccountSlot) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, AccountPoolMessage::Submit(cookie)).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with AccountPoolActor for submit operation: {e}"
                ),
            }
        })
    }

    pub async fn get_status(&self) -> Result<AccountPoolStatus, ClewdrError> {
        ractor::call!(self.actor_ref, AccountPoolMessage::GetStatus).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with AccountPoolActor for get status operation: {e}"
                ),
            }
        })
    }

    pub async fn reload_from_db(&self) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, AccountPoolMessage::ReloadFromDb).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with AccountPoolActor for reload operation: {e}"
                ),
            }
        })
    }

    pub async fn probe_all(&self) -> Result<Vec<i64>, ClewdrError> {
        ractor::call!(self.actor_ref, AccountPoolMessage::ProbeAll).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with AccountPoolActor for probe operation: {e}"
                ),
            }
        })
    }

    pub async fn probe_accounts(
        &self,
        account_ids: Vec<i64>,
        event_tx: broadcast::Sender<AdminEvent>,
    ) -> Result<Vec<i64>, ClewdrError> {
        ractor::call!(
            self.actor_ref,
            AccountPoolMessage::ProbeAccounts,
            account_ids,
            event_tx
        )
        .map_err(|e| ClewdrError::RactorError {
            loc: Location::generate(),
            msg: format!("Failed to communicate with AccountPoolActor for targeted probe: {e}"),
        })
    }

    pub async fn begin_probe(&self, account_id: i64) -> Result<bool, ClewdrError> {
        ractor::call!(self.actor_ref, AccountPoolMessage::BeginProbe, account_id).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with AccountPoolActor for begin probe: {e}"),
            }
        })
    }

    pub async fn release_slot(&self, account_id: i64) {
        let _ = ractor::cast!(self.actor_ref, AccountPoolMessage::ReleaseSlot(account_id));
    }

    pub async fn get_probing_ids(&self) -> Result<Vec<i64>, ClewdrError> {
        ractor::call!(self.actor_ref, AccountPoolMessage::GetProbingIds).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with AccountPoolActor for get probing ids: {e}"
                ),
            }
        })
    }

    pub async fn clear_probing(&self, account_id: i64) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, AccountPoolMessage::ClearProbing(account_id)).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with AccountPoolActor for clear probing: {e}"),
            }
        })
    }

    pub async fn set_probe_error(&self, account_id: i64, msg: String) {
        let _ = ractor::cast!(
            self.actor_ref,
            AccountPoolMessage::SetProbeError(account_id, msg)
        );
    }

    pub async fn clear_probe_error(&self, account_id: i64) {
        let _ = ractor::cast!(
            self.actor_ref,
            AccountPoolMessage::ClearProbeError(account_id)
        );
    }

    pub async fn get_probe_errors(&self) -> Result<HashMap<i64, String>, ClewdrError> {
        ractor::call!(self.actor_ref, AccountPoolMessage::GetProbeErrors).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with AccountPoolActor for get probe errors: {e}"
                ),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountPoolActor, is_oauth_placeholder_slot, oauth_placeholder_cookie};
    use std::str::FromStr;
    use std::time::Duration;

    use crate::config::{AccountSlot, ClewdrCookie, TokenInfo};

    #[test]
    fn oauth_placeholder_cookie_is_unique_per_account_and_accepted_by_parser() {
        // The synthesized placeholder must (a) satisfy `ClewdrCookie::from_str`'s
        // regex so the loader can construct an `AccountSlot`, and (b) be distinct
        // per account_id so slots remain hashable/equal-distinct in the pool's
        // HashSet<AccountSlot> (exhausted) and moka affinity cache.
        let c1 = oauth_placeholder_cookie(1);
        let c2 = oauth_placeholder_cookie(2);
        let c_big = oauth_placeholder_cookie(i64::MAX);

        assert_ne!(c1, c2);
        assert_ne!(c1, c_big);
        for raw in [&c1, &c2, &c_big] {
            ClewdrCookie::from_str(raw)
                .unwrap_or_else(|e| panic!("placeholder {raw:?} failed regex: {e}"));
        }
    }

    #[test]
    fn oauth_placeholder_detection_distinguishes_synthetic_from_real_cookies() {
        // The detector is what keeps cookie-style probes (`probe_cookie`) from
        // running against oauth-only slots. If a real cookie accidentally matches
        // the placeholder pattern, probes would be skipped for a real account,
        // so the detector must stay tight.
        let synthetic = AccountSlot::new(&oauth_placeholder_cookie(42), None).unwrap();
        assert!(is_oauth_placeholder_slot(&synthetic));

        // Shape of a real-looking Claude session cookie — uses sid01, not sid99.
        let real_raw = format!("sk-ant-sid01-{}-abcdefAA", "a".repeat(86));
        let real = AccountSlot::new(&real_raw, None).unwrap();
        assert!(!is_oauth_placeholder_slot(&real));
    }

    #[test]
    fn in_memory_runtime_merge_keeps_db_oauth_token_when_present() {
        let mut reloaded = AccountSlot::new(&oauth_placeholder_cookie(7), None).unwrap();
        reloaded.token = Some(TokenInfo::from_parts(
            "db-access".to_string(),
            "db-refresh".to_string(),
            Duration::from_secs(3600),
            "org-db".to_string(),
        ));

        let mut mem = AccountSlot::new(&oauth_placeholder_cookie(7), None).unwrap();
        mem.token = Some(TokenInfo::from_parts(
            "mem-access".to_string(),
            "mem-refresh".to_string(),
            Duration::from_secs(3600),
            "org-mem".to_string(),
        ));
        mem.email = Some("mem@example.com".to_string());

        AccountPoolActor::apply_in_memory_runtime(&mut reloaded, mem, false);

        assert_eq!(
            reloaded
                .token
                .as_ref()
                .map(|token| token.access_token.as_str()),
            Some("db-access")
        );
        assert_eq!(reloaded.email.as_deref(), Some("mem@example.com"));
    }
}
