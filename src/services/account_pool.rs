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
    config::{AccountSlot, ClewdrCookie, InvalidAccountSlot, Reason, TokenInfo, UsageBreakdown},
    db::accounts::{
        AccountSummary, active_reset_time, batch_upsert_runtime_states, load_all_accounts,
        set_account_disabled, set_accounts_active, summarize_accounts,
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
    /// Update the cached OAuth credential for an account without marking it
    /// dirty. Used by refresh/probe paths that already wrote the authoritative
    /// token to DB — this only keeps the in-memory slot in sync so subsequent
    /// dispatches don't hand out a stale token.
    UpdateCredential(i64, Option<TokenInfo>),
    /// Read the currently cached OAuth token for an account from the pool's
    /// in-memory slot. Used by refresh callers to re-check (after acquiring the
    /// per-account refresh guard) whether a peer already refreshed the token.
    GetToken(i64, RpcReplyPort<Option<TokenInfo>>),
    /// Converge the in-memory pool for an account whose status has already
    /// been persisted to DB by an explicit write path (e.g.
    /// `set_account_auth_error`, `set_account_disabled`). This message does
    /// **not** mark the account dirty — persisting status is the caller's
    /// responsibility, `do_flush` must not touch the authoritative status by
    /// way of `state.invalid`.
    Invalidate(i64, Reason),
}

#[derive(Debug)]
struct AccountPoolState {
    valid: VecDeque<AccountSlot>,
    exhausted: HashMap<i64, AccountSlot>,
    invalid: HashSet<InvalidAccountSlot>,
    moka: Cache<u64, i64>,
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

    /// Update the cached OAuth token for an account in both `valid` and
    /// `exhausted`. The authoritative DB write is expected to have already
    /// happened on the caller's side — this only keeps the in-memory slot in
    /// sync so subsequent dispatches don't hand out a stale credential.
    ///
    /// Does not mark the account dirty: the runtime flush must never write
    /// credential columns, per `docs/account-normalization-2026-04-21.md`
    /// ("凭证类字段以 DB 为准").
    fn update_slot_credential(
        state: &mut AccountPoolState,
        account_id: i64,
        token: Option<TokenInfo>,
    ) {
        for slot in state.valid.iter_mut() {
            if slot.account_id == Some(account_id) {
                slot.token = token.clone();
            }
        }

        if let Some(slot) = state.exhausted.get_mut(&account_id) {
            slot.token = token;
        }
    }

    /// In-memory convergence for an account whose authoritative status was
    /// just written to DB by an explicit path (`set_account_auth_error`,
    /// `set_account_disabled`, or similar). Removes the account from dispatch
    /// surfaces, wipes affinity entries pointing at it, and records it in
    /// `state.invalid` so pool-view summaries reflect DB reality.
    ///
    /// Deliberately does **not** call `mark_dirty`: the status was already
    /// persisted by the caller, and letting `do_flush` also touch status would
    /// let the pool's Reason race with the DB's (`auth_error` vs `disabled`).
    /// See `docs/account-normalization-2026-04-21.md` §"容易漏掉 #5" for the
    /// broader principle.
    fn converge_invalidate(state: &mut AccountPoolState, account_id: i64, reason: Reason) {
        // Remove from valid.
        let mut removed_cookie: Option<ClewdrCookie> = None;
        state.valid.retain(|c| {
            if c.account_id == Some(account_id) {
                if removed_cookie.is_none() {
                    removed_cookie = Some(c.cookie.clone());
                }
                false
            } else {
                true
            }
        });

        // Remove from exhausted (direct id lookup — exhausted is keyed by account_id).
        if let Some(slot) = state.exhausted.remove(&account_id) {
            removed_cookie.get_or_insert(slot.cookie);
        }

        // Record in invalid so pool-view summaries and collect's sticky-reason
        // guard see the authoritative reason. Existing entry (if any) is
        // replaced so the reason reflects the latest cause.
        if let Some(cookie) = removed_cookie {
            let marker = InvalidAccountSlot::new(cookie.clone(), Reason::Null);
            let existing = state.invalid.take(&marker);
            state.invalid.insert(InvalidAccountSlot::with_account_id(
                cookie,
                reason,
                Some(account_id),
            ));
            drop(existing);
        }

        // Stop advertising the account for preferred-drain dispatch.
        state.drain_first_ids.remove(&account_id);

        // Detach the account from every flush-driven DB status write so the
        // authoritative status just written by the caller cannot be raced:
        //   - `reactivated` would cause `set_accounts_active` to flip it back
        //     to "active".
        //   - `dirty` combined with an entry in `state.invalid` would cause
        //     `set_account_disabled(id, reason.to_db_string())` to overwrite
        //     an `auth_error` row with `disabled`. Runtime-state flushing
        //     only scans `valid` + `exhausted` (neither contains this account
        //     anymore), so dropping the account from `dirty` loses nothing
        //     meaningful.
        state.reactivated.remove(&account_id);
        state.dirty.remove(&account_id);

        // Wipe affinity entries pointing at this account_id so coding sessions
        // rebind on the next request.
        state
            .moka
            .invalidate_entries_if(move |_, v| *v == account_id)
            .ok();

        // Inflight is intentionally left alone: in-flight Return / ReleaseSlot
        // messages still arrive for this account and must decrement the
        // counter. The collect sticky-reason guard prevents those Returns from
        // flipping the account back into `valid`.

        Self::emit_accounts_refresh(state);
    }

    fn mark_all_dirty(state: &mut AccountPoolState) {
        for cs in state.valid.iter().chain(state.exhausted.values()) {
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
        state.exhausted.retain(|_, cookie| {
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
            for cookie in state.exhausted.values_mut() {
                if apply_resets(cookie) {
                    changed = true;
                    if let Some(id) = cookie.account_id {
                        dirty_from_exhausted.push(id);
                    }
                }
            }
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

        // --- predicates ---
        let bound_ok = |id: i64| -> bool { bound.is_empty() || bound.contains(&id) };
        let slot_ok = |id: i64, inflight: &HashMap<i64, (u32, u32)>| -> bool {
            inflight.get(&id).is_none_or(|(cur, max)| cur < max)
        };
        let is_usable = |c: &AccountSlot, inflight: &HashMap<i64, (u32, u32)>| -> bool {
            c.account_id
                .is_some_and(|id| bound_ok(id) && slot_ok(id, inflight))
        };

        // ---------- Phase A: affinity ----------
        // Check the prompt-hash → account_id binding first. If the cached
        // account is usable right now, return it without touching the cache
        // (no insert). If it's unusable only because its inflight slots are
        // saturated, we overflow-borrow another drain_first (or regular)
        // account — but we do NOT rewrite the cache, so affinity stays with
        // the original once it frees up. The cache is only invalidated when
        // the cached account has been removed from `valid` (Invalidate, delete,
        // or bound mismatch) — in that case we fall through to B/C which
        // rewrites it.
        let cached_id = cache_key.and_then(|k| state.moka.get(&k));
        if let Some(cached_id) = cached_id {
            let cached_pos = state
                .valid
                .iter()
                .position(|c| c.account_id == Some(cached_id));
            match cached_pos {
                None => {
                    // cached in moka but no longer in valid (Invalidate'd /
                    // account removed / filtered by bound). Let B/C pick a
                    // fresh account and rewrite the cache.
                    if let Some(k) = cache_key {
                        state.moka.invalidate(&k);
                    }
                }
                Some(pos) => {
                    if is_usable(&state.valid[pos], &state.inflight) {
                        return Self::commit_dispatch(state, pos, cache_key, false);
                    }
                    if !bound_ok(cached_id) {
                        // Cached doesn't match this request's bound set — treat
                        // as stale. Drop cache, fall through to B/C to bind to
                        // an in-bound account.
                        if let Some(k) = cache_key {
                            state.moka.invalidate(&k);
                        }
                    } else {
                        // Only inflight saturation — overflow-borrow a sibling
                        // (drain_first preferred) without touching the cache.
                        let borrow_pos = state
                            .valid
                            .iter()
                            .position(|c| {
                                c.account_id != Some(cached_id)
                                    && is_usable(c, &state.inflight)
                                    && c.account_id
                                        .is_some_and(|id| state.drain_first_ids.contains(&id))
                            })
                            .or_else(|| {
                                state.valid.iter().position(|c| {
                                    c.account_id != Some(cached_id) && is_usable(c, &state.inflight)
                                })
                            });
                        return match borrow_pos {
                            Some(pos) => Self::commit_dispatch(state, pos, cache_key, false),
                            None => Err(Self::dispatch_empty_error(state, bound)),
                        };
                    }
                }
            }
        }

        // ---------- Phase B: prefer drain_first accounts ----------
        if !state.drain_first_ids.is_empty()
            && let Some(pos) = state.valid.iter().position(|c| {
                is_usable(c, &state.inflight)
                    && c.account_id
                        .is_some_and(|id| state.drain_first_ids.contains(&id))
            })
        {
            return Self::commit_dispatch(state, pos, cache_key, true);
        }

        // ---------- Phase C: round-robin ----------
        if let Some(pos) = state
            .valid
            .iter()
            .position(|c| is_usable(c, &state.inflight))
        {
            return Self::commit_dispatch(state, pos, cache_key, true);
        }

        Err(Self::dispatch_empty_error(state, bound))
    }

    /// Remove the slot at `pos` from `valid`, increment inflight, re-queue at
    /// the back (round-robin), and optionally rewrite the affinity cache.
    fn commit_dispatch(
        state: &mut AccountPoolState,
        pos: usize,
        cache_key: Option<u64>,
        rewrite_cache: bool,
    ) -> Result<AccountSlot, ClewdrError> {
        let cookie = state.valid.remove(pos).unwrap();
        if let Some(aid) = cookie.account_id
            && let Some((cur, _)) = state.inflight.get_mut(&aid)
        {
            *cur += 1;
        }
        state.valid.push_back(cookie.clone());
        if rewrite_cache
            && let Some(key) = cache_key
            && let Some(aid) = cookie.account_id
        {
            state.moka.insert(key, aid);
        }
        Ok(cookie)
    }

    /// Classify dispatch failure: if any in-bound account is still in `valid`
    /// or `exhausted` we return `UpstreamCoolingDown` (transient); otherwise
    /// there is no account to serve at all → `NoValidUpstreamAccounts`.
    fn dispatch_empty_error(state: &AccountPoolState, bound: &[i64]) -> ClewdrError {
        let has_relevant_valid = state
            .valid
            .iter()
            .any(|c| bound.is_empty() || c.account_id.is_some_and(|id| bound.contains(&id)));
        let has_relevant_exhausted = state
            .exhausted
            .values()
            .any(|c| bound.is_empty() || c.account_id.is_some_and(|id| bound.contains(&id)));
        if has_relevant_valid || has_relevant_exhausted {
            ClewdrError::UpstreamCoolingDown
        } else {
            ClewdrError::NoValidUpstreamAccounts
        }
    }

    fn collect(state: &mut AccountPoolState, cookie: AccountSlot, reason: Option<Reason>) -> bool {
        let aid = cookie.account_id;

        let removed_probe = aid.is_some_and(|id| state.probing.remove(&id));

        let tmp = InvalidAccountSlot::new(cookie.cookie.clone(), Reason::Null);

        // Sticky-reason guard: accounts explicitly invalidated (via the
        // `Invalidate` message from an auth_error / disabled path, or via a
        // DB reload that surfaced one of those states) must not be
        // auto-reactivated by a Return from an in-flight request that predates
        // the invalidation. TMR / Restricted remain transient — the cooldown
        // reactivation path below stays intact for those reasons.
        if let Some(existing) = state.invalid.get(&tmp)
            && matches!(
                existing.reason,
                Reason::Free | Reason::Disabled | Reason::Banned | Reason::Null
            )
        {
            // Silent drop: the DB already reflects the authoritative reason,
            // the inflight counter is decremented independently by
            // `ReleaseSlot`, and no SSE event is owed — callers that care
            // have already seen the status change via the explicit DB write
            // path's own notification.
            return removed_probe;
        }

        // Remove from whichever set the cookie currently lives in
        let was_valid = state
            .valid
            .iter()
            .position(|c| *c == cookie)
            .map(|i| state.valid.remove(i).unwrap());
        let was_exhausted = aid.and_then(|id| state.exhausted.remove(&id));
        let was_invalid = state.invalid.take(&tmp);

        if was_valid.is_none() && was_exhausted.is_none() && was_invalid.is_none() {
            return removed_probe;
        }

        let changed_set = match &reason {
            None => {
                if cookie.reset_time.is_some() {
                    let was_ex = was_exhausted.is_some();
                    if let Some(id) = aid {
                        state.exhausted.insert(id, cookie);
                    }
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
                if let Some(id) = aid {
                    state.exhausted.insert(id, cookie);
                }
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
            || state.exhausted.values().any(|c| c == &cookie)
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
            .chain(state.exhausted.values().cloned())
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
            .chain(state.exhausted.values().cloned())
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
            exhausted: state.exhausted.values().cloned().collect(),
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

        // Build runtime-state updates only. Credential fields
        // (`oauth_access_token / oauth_refresh_token / oauth_expires_at`) must
        // not be written from the in-memory slot — doing so can overwrite a
        // freshly-rotated refresh token with the stale copy that the pool has
        // not seen yet. Credentials follow the rule "DB is authoritative" per
        // `docs/account-normalization-2026-04-21.md`; the dedicated refresh
        // paths (probe, chat's `persist_oauth_refresh`, admin test) write DB
        // themselves and call `update_credential` to sync the in-memory slot.
        let mut params = Vec::new();
        for cs in state.valid.iter().chain(state.exhausted.values()) {
            if let Some(id) = cs.account_id
                && dirty_ids.contains(&id)
            {
                params.push((id, cs.to_runtime_params()));
            }
        }

        let mut disabled = Vec::new();
        for uc in &state.invalid {
            if let Some(id) = uc.account_id
                && dirty_ids.contains(&id)
            {
                disabled.push((id, uc.reason.to_db_string()));
            }
        }

        // Await directly — 1-3 accounts, <1ms. On failure, re-insert dirty IDs.
        if let Err(e) = batch_upsert_runtime_states(&state.db, &params).await {
            warn!("Failed to flush runtime states: {e}");
            for (id, _) in &params {
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
        for (id, cs) in state.exhausted.drain() {
            mem_cookies.insert(id, cs);
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
                state.exhausted.insert(row.id, cs);
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
            .support_invalidation_closures()
            .build();

        let mut state = AccountPoolState {
            valid: VecDeque::new(),
            exhausted: HashMap::new(),
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
            AccountPoolMessage::UpdateCredential(account_id, token) => {
                Self::update_slot_credential(state, account_id, token);
            }
            AccountPoolMessage::GetToken(account_id, reply_port) => {
                let token = state
                    .valid
                    .iter()
                    .chain(state.exhausted.values())
                    .find(|c| c.account_id == Some(account_id))
                    .and_then(|c| c.token.clone());
                reply_port.send(token)?;
            }
            AccountPoolMessage::Invalidate(account_id, reason) => {
                Self::converge_invalidate(state, account_id, reason);
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
        for cs in state.valid.iter().chain(state.exhausted.values()) {
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

    /// Push a freshly-refreshed OAuth token into the in-memory pool slot so
    /// subsequent dispatches hand out the new credential. The authoritative DB
    /// write must have happened on the caller's side first.
    pub async fn update_credential(&self, account_id: i64, token: Option<TokenInfo>) {
        let _ = ractor::cast!(
            self.actor_ref,
            AccountPoolMessage::UpdateCredential(account_id, token)
        );
    }

    /// Read the currently cached OAuth token for an account from the pool's
    /// in-memory slot. Used by refresh call sites (after acquiring the
    /// per-account refresh guard) to decide whether a peer already refreshed
    /// the token and the current caller can skip the upstream call.
    pub async fn get_token(&self, account_id: i64) -> Result<Option<TokenInfo>, ClewdrError> {
        ractor::call!(self.actor_ref, AccountPoolMessage::GetToken, account_id).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with AccountPoolActor for get token: {e}"),
            }
        })
    }

    /// Converge the in-memory pool after an explicit DB status write
    /// (auth_error, disabled, banned, etc.). Does not persist status — the
    /// caller is expected to have already written it via the appropriate
    /// `set_account_*` helper. See `AccountPoolActor::converge_invalidate`.
    pub async fn invalidate(&self, account_id: i64, reason: Reason) {
        let _ = ractor::cast!(
            self.actor_ref,
            AccountPoolMessage::Invalidate(account_id, reason)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccountPoolActor, AccountPoolState, is_oauth_placeholder_slot, oauth_placeholder_cookie,
    };
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::str::FromStr;
    use std::time::Duration;

    use moka::sync::Cache;
    use tokio::sync::broadcast;

    use crate::config::{AccountSlot, ClewdrCookie, Reason, TokenInfo};
    use crate::db::init_pool;

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

    fn empty_state(db: sqlx::SqlitePool) -> AccountPoolState {
        let (event_tx, _rx) = broadcast::channel(16);
        let moka = Cache::builder()
            .max_capacity(1000)
            .time_to_idle(std::time::Duration::from_secs(60 * 60))
            .support_invalidation_closures()
            .build();
        AccountPoolState {
            valid: VecDeque::new(),
            exhausted: HashMap::new(),
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
        }
    }

    fn token_with_refresh(refresh: &str) -> TokenInfo {
        TokenInfo::from_parts(
            "stale-at".to_string(),
            refresh.to_string(),
            Duration::from_secs(3600),
            "org".to_string(),
        )
    }

    async fn insert_oauth_row(pool: &sqlx::SqlitePool, id: i64, access: &str, refresh: &str) {
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source,
                oauth_access_token, oauth_refresh_token, oauth_expires_at, drain_first
            ) VALUES (?1, ?2, 1, 5, 'active', 'oauth', ?3, ?4, '2030-01-01T00:00:00Z', 0)",
        )
        .bind(id)
        .bind(format!("acc-{id}"))
        .bind(access)
        .bind(refresh)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn read_refresh_token(pool: &sqlx::SqlitePool, id: i64) -> String {
        let row: (Option<String>,) =
            sqlx::query_as("SELECT oauth_refresh_token FROM accounts WHERE id = ?1")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
        row.0.unwrap_or_default()
    }

    /// Regression for the 2026-04-22 production incident: after a probe rotated
    /// the refresh token (via `upsert_account_oauth` directly), the pool's
    /// periodic `do_flush` was writing the *stale* in-memory slot's token back
    /// into the DB, invalidating the rotation. `do_flush` must no longer touch
    /// credential columns.
    #[tokio::test]
    async fn probe_success_does_not_overwrite_rt_on_flush() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        insert_oauth_row(&pool, 1, "at0", "rt0").await;

        let mut state = empty_state(pool.clone());
        let mut slot = AccountSlot::new(&oauth_placeholder_cookie(1), None).unwrap();
        slot.account_id = Some(1);
        slot.token = Some(token_with_refresh("rt0"));
        state.valid.push_back(slot);

        // A concurrent refresh (probe or chat) rotated the token in DB to rt1
        // without telling the pool's in-memory slot.
        let rotated = TokenInfo::from_parts(
            "at1".to_string(),
            "rt1".to_string(),
            Duration::from_secs(3600),
            "org".to_string(),
        );
        crate::db::accounts::upsert_account_oauth(&pool, 1, Some(&rotated), None)
            .await
            .unwrap();
        assert_eq!(read_refresh_token(&pool, 1).await, "rt1");

        // Simulate any runtime-state change that would mark the account dirty.
        AccountPoolActor::mark_dirty(&mut state, Some(1));
        AccountPoolActor::do_flush(&mut state).await;

        // do_flush must not have clobbered the freshly-rotated refresh token.
        assert_eq!(
            read_refresh_token(&pool, 1).await,
            "rt1",
            "do_flush must not overwrite oauth_refresh_token from stale in-memory slot"
        );
    }

    #[tokio::test]
    async fn update_slot_credential_replaces_in_memory_token() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        insert_oauth_row(&pool, 1, "at0", "rt0").await;

        let mut state = empty_state(pool);
        let mut slot = AccountSlot::new(&oauth_placeholder_cookie(1), None).unwrap();
        slot.account_id = Some(1);
        slot.token = Some(token_with_refresh("rt0"));
        state.valid.push_back(slot);

        AccountPoolActor::update_slot_credential(&mut state, 1, Some(token_with_refresh("rt1")));

        let updated = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(1))
            .and_then(|c| c.token.as_ref())
            .map(|t| t.refresh_token.clone());
        assert_eq!(updated.as_deref(), Some("rt1"));
        // No dirty marking — flush should not write token via this path.
        assert!(state.dirty.is_empty());
    }

    // Compile-time assertion that the affinity cache stores account_id, not a
    // full AccountSlot. Guards against regressing Bug 1's fix.
    #[allow(dead_code)]
    fn _assert_moka_cache_type_is_account_id(s: &AccountPoolState) {
        let _: &Cache<u64, i64> = &s.moka;
    }

    fn push_slot(state: &mut AccountPoolState, id: i64, max_slots: u32) {
        let mut slot = AccountSlot::new(&oauth_placeholder_cookie(id), None).unwrap();
        slot.account_id = Some(id);
        slot.token = Some(token_with_refresh(&format!("rt-{id}")));
        state.inflight.insert(id, (0, max_slots));
        state.valid.push_back(slot);
    }

    /// Bug 1 regression: an inflight-saturated `drain_first` account that is
    /// currently bound in the affinity cache must not cause the cache to
    /// rebind when the dispatcher overflows to another drain_first sibling.
    /// "Slot full is overflow, not rebinding."
    #[tokio::test]
    async fn cached_drain_first_inflight_full_borrows_without_rebind() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);
        push_slot(&mut state, 1, 1); // A (drain_first)
        push_slot(&mut state, 2, 1); // B (drain_first)
        state.drain_first_ids.insert(1);
        state.drain_first_ids.insert(2);
        // Cached binding: key=77 → account 1.
        state.moka.insert(77, 1);
        // Saturate account 1's inflight.
        state.inflight.insert(1, (1, 1));

        let actor = AccountPoolActor;
        let dispatched = actor.dispatch(&mut state, Some(77), &[]).unwrap();

        assert_eq!(dispatched.account_id, Some(2), "should overflow to B");
        state.moka.run_pending_tasks();
        assert_eq!(
            state.moka.get(&77),
            Some(1),
            "cache must remain bound to A — slot-full is overflow, not rebinding"
        );
    }

    /// A cached binding to an account that has been invalidated (removed from
    /// `state.valid` by Invalidate or account deletion) must rebind on the
    /// next dispatch. The cache entry is cleared and the new winner is
    /// written back.
    #[tokio::test]
    async fn cached_auth_error_triggers_rebind() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);
        push_slot(&mut state, 1, 5);
        push_slot(&mut state, 2, 5);
        state.moka.insert(77, 1);
        // Simulate auth_error: account 1 explicitly invalidated.
        AccountPoolActor::converge_invalidate(&mut state, 1, Reason::Null);

        let actor = AccountPoolActor;
        let dispatched = actor.dispatch(&mut state, Some(77), &[]).unwrap();

        assert_eq!(dispatched.account_id, Some(2), "must rebind to B");
        state.moka.run_pending_tasks();
        assert_eq!(state.moka.get(&77), Some(2), "cache must point at B now");
    }

    /// `Invalidate` must wipe every affinity entry pointing at the removed
    /// account, not just the key the current request used.
    #[tokio::test]
    async fn invalidate_clears_moka_entries_for_account() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);
        push_slot(&mut state, 1, 5);
        push_slot(&mut state, 2, 5);
        state.moka.insert(10, 1);
        state.moka.insert(11, 1);
        state.moka.insert(12, 2);
        state.moka.run_pending_tasks();

        AccountPoolActor::converge_invalidate(&mut state, 1, Reason::Null);
        // `invalidate_entries_if` in moka 0.12 is processed asynchronously;
        // force the scheduled deletions through before asserting.
        state.moka.run_pending_tasks();

        assert_eq!(state.moka.get(&10), None, "key 10 → A must be cleared");
        assert_eq!(state.moka.get(&11), None, "key 11 → A must be cleared");
        assert_eq!(
            state.moka.get(&12),
            Some(2),
            "key 12 → B must not be touched"
        );
    }

    /// A Return from an in-flight request whose account was explicitly
    /// invalidated with a sticky reason (auth_error / disabled / banned /
    /// free / null) must not auto-reactivate the account. The DB is
    /// authoritative; pool must not silently flip status back to active via
    /// `state.reactivated` → `set_accounts_active`.
    #[tokio::test]
    async fn collect_skips_reactivation_for_sticky_invalid_reason() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);

        let mut slot = AccountSlot::new(&oauth_placeholder_cookie(1), None).unwrap();
        slot.account_id = Some(1);
        // Account is sitting in `invalid` with a sticky reason (auth_error
        // reloaded → Reason::Null).
        state
            .invalid
            .insert(crate::config::InvalidAccountSlot::with_account_id(
                slot.cookie.clone(),
                Reason::Null,
                Some(1),
            ));

        // In-flight request returns successfully (reason=None) — the pre-fix
        // behaviour would take from invalid and push back into valid, then
        // mark `state.reactivated` which drives `set_accounts_active` in
        // do_flush, clobbering the DB auth_error.
        AccountPoolActor::collect(&mut state, slot.clone(), None);

        assert!(
            state.invalid.iter().any(|u| u.account_id == Some(1)),
            "sticky-invalidated account must remain in state.invalid"
        );
        assert!(
            !state.valid.iter().any(|c| c.account_id == Some(1)),
            "must not be reinserted into valid"
        );
        assert!(
            !state.reactivated.contains(&1),
            "must not queue DB reactivation"
        );
    }

    /// Counter-test for the sticky-reason guard: cooldown reasons
    /// (TooManyRequest / Restricted) must still be allowed to auto-reactivate
    /// when a later Return arrives with reason=None. This is the existing
    /// "account cooled down, back in service" flow.
    #[tokio::test]
    async fn collect_still_reactivates_for_cooldown_reason() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);

        let mut slot = AccountSlot::new(&oauth_placeholder_cookie(2), None).unwrap();
        slot.account_id = Some(2);
        state
            .invalid
            .insert(crate::config::InvalidAccountSlot::with_account_id(
                slot.cookie.clone(),
                Reason::TooManyRequest(1_700_000_000),
                Some(2),
            ));

        AccountPoolActor::collect(&mut state, slot.clone(), None);

        assert!(
            state.valid.iter().any(|c| c.account_id == Some(2)),
            "TMR-invalidated account must still reactivate on normal return"
        );
        assert!(
            state.reactivated.contains(&2),
            "TMR reactivation must queue set_accounts_active"
        );
    }

    /// Regression for the ordering hazard called out in code review: a prior
    /// TMR/Restricted return queued the account into `state.reactivated` (and
    /// via `collect`'s `mark_dirty`, also into `state.dirty`). An
    /// auth_error / disabled path then writes the authoritative DB status and
    /// invalidates the pool. Both the pending reactivation AND the dirty
    /// marking must be dropped so `do_flush` does not race the freshly-
    /// written auth_error with either `set_accounts_active` (via
    /// `state.reactivated`) or `set_account_disabled` (via
    /// `state.invalid + state.dirty`).
    #[tokio::test]
    async fn invalidate_discards_pending_flush_side_effects() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        insert_oauth_row(&pool, 1, "at0", "rt0").await;
        // Seed the authoritative auth_error that a probe would have written.
        crate::db::accounts::set_account_auth_error(&pool, 1, "probe failure")
            .await
            .unwrap();

        let mut state = empty_state(pool.clone());

        // Simulate a prior TMR return that cool-down-reactivated the account:
        // slot is back in `valid`, `reactivated` queues DB set-active, and
        // `collect` marked the account dirty.
        let mut slot = AccountSlot::new(&oauth_placeholder_cookie(1), None).unwrap();
        slot.account_id = Some(1);
        state
            .invalid
            .insert(crate::config::InvalidAccountSlot::with_account_id(
                slot.cookie.clone(),
                Reason::TooManyRequest(1_700_000_000),
                Some(1),
            ));
        AccountPoolActor::collect(&mut state, slot.clone(), None);
        assert!(state.reactivated.contains(&1));
        assert!(state.dirty.contains(&1));
        assert!(state.valid.iter().any(|c| c.account_id == Some(1)));

        // Explicit failure path: probe writes auth_error to DB, then converges
        // the pool. Both queued flush side-effects must be cleared.
        AccountPoolActor::converge_invalidate(&mut state, 1, Reason::Null);
        assert!(
            !state.reactivated.contains(&1),
            "reactivated must be cleared"
        );
        assert!(!state.dirty.contains(&1), "dirty must be cleared");
        assert!(!state.valid.iter().any(|c| c.account_id == Some(1)));
        assert!(state.invalid.iter().any(|u| u.account_id == Some(1)));

        // Flushing must not touch the account at all — DB status stays at the
        // value the explicit write path just set.
        AccountPoolActor::do_flush(&mut state).await;

        let (status,): (String,) = sqlx::query_as("SELECT status FROM accounts WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "auth_error",
            "do_flush must not race the authoritative auth_error write"
        );
    }

    /// PR-review regression: after a probe rotates the refresh token on an
    /// `auth_error` account, the refresh is persisted to DB while the pool
    /// still has the account in `state.invalid`. A subsequent queued probe
    /// / test on the same account must read the rotated RT from DB via the
    /// guard's fallback path, not the pre-guard clone. Today `get_token`
    /// only scans `valid + exhausted`, so on a pool miss callers MUST
    /// re-read DB — this test pins the in-pool side of that contract
    /// (`get_token` returns None for invalid accounts) so the callsite
    /// fallback remains load-bearing.
    #[tokio::test]
    async fn get_token_returns_none_for_invalidated_account() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        insert_oauth_row(&pool, 1, "at0", "rt0").await;
        let mut state = empty_state(pool);
        let mut slot = AccountSlot::new(&oauth_placeholder_cookie(1), None).unwrap();
        slot.account_id = Some(1);
        slot.token = Some(token_with_refresh("rt0"));
        state.valid.push_back(slot);

        // Seed sentinel: get_token sees the slot while it's in `valid`.
        let seen = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(1))
            .and_then(|c| c.token.as_ref())
            .map(|t| t.refresh_token.clone());
        assert_eq!(seen.as_deref(), Some("rt0"));

        // Moving the account to `state.invalid` (via Invalidate) drops the
        // token from the pool's searchable sets. Callers must fall back to
        // DB under their guard instead of using a pre-guard clone.
        AccountPoolActor::converge_invalidate(&mut state, 1, Reason::Null);
        let after = state
            .valid
            .iter()
            .chain(state.exhausted.values())
            .find(|c| c.account_id == Some(1))
            .and_then(|c| c.token.as_ref())
            .map(|t| t.refresh_token.clone());
        assert_eq!(
            after, None,
            "get_token's data source must miss for invalidated accounts — callers rely on DB fallback"
        );
    }
}
