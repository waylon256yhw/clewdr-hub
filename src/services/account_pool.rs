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
    claude_code_state::probe::{probe_cookie, probe_oauth_account},
    config::{
        AccountSlot, AuthMethod, InvalidAccountSlot, Reason, RuntimeStateParams, TokenInfo,
        UsageBreakdown,
    },
    db::accounts::{
        AccountWithRuntime, active_reset_time, batch_upsert_runtime_states, get_account_by_id,
        load_all_accounts, set_account_disabled, set_accounts_active,
    },
    error::ClewdrError,
    services::account_health::{
        AccountHealthSnapshot, AccountHealthSummary, PoolSnapshotView, compose_health_snapshot,
    },
    state::AdminEvent,
    stealth,
};

const INTERVAL: u64 = 300;
const FLUSH_INTERVAL: u64 = 15;
const SESSION_WINDOW_SECS: i64 = 5 * 60 * 60; // 5h
const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 60 * 60; // 7d

/// Length of the credential prefix used for fingerprinting in C5's
/// release_runtime guard. 20 bytes is enough to distinguish admin
/// replacements (cookie blobs and refresh tokens are both 80+ chars with
/// high-entropy first bytes) without bloating the message payload that
/// flows through every chat / probe completion.
const CREDENTIAL_FINGERPRINT_LEN: usize = 20;

/// Stable identity for a credential at request-acquire time. Captured by
/// every caller of `release_runtime` so `collect_by_id` can detect that
/// the pool's slot has been credential-rotated (admin replacement) since
/// the request started, and discard the stale runtime / Reason instead of
/// applying it to a slot that no longer represents the same logical
/// credential.
///
/// OAuth uses the **refresh_token** prefix, not access_token: a normal
/// OAuth refresh rotates `access_token` but keeps `refresh_token`, so the
/// fingerprint must survive `refresh_token`-stable rotations or every
/// request that overlaps a refresh would falsely trip the guard. Admin
/// reconnect rotates both, so the fingerprint correctly flips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialFingerprint {
    Cookie(String),
    OAuth(String),
}

impl CredentialFingerprint {
    fn cookie_prefix(s: &str) -> Self {
        let cap = CREDENTIAL_FINGERPRINT_LEN.min(s.len());
        CredentialFingerprint::Cookie(s[..cap].to_string())
    }

    fn oauth_prefix(s: &str) -> Self {
        let cap = CREDENTIAL_FINGERPRINT_LEN.min(s.len());
        CredentialFingerprint::OAuth(s[..cap].to_string())
    }

    pub fn from_oauth_refresh_token(refresh_token: &str) -> Self {
        Self::oauth_prefix(refresh_token)
    }

    /// Build a fingerprint from a request-time `AccountSlot`. Returns None
    /// when the slot has no usable credential identifier (an OAuth slot
    /// with `token = None` — should not happen in practice, but treated
    /// as "no fingerprint" so the caller's guard becomes a pass-through
    /// rather than a false rejection).
    pub fn from_slot(slot: &AccountSlot) -> Option<Self> {
        match slot.auth_method {
            // Use the inner cookie blob (`Deref<Target = str>`), not
            // `to_string()` — the latter prepends `sessionKey=` which is
            // identical across every cookie account and would collapse
            // every fingerprint into the same 20-byte prefix. Cookie kind
            // invariant: post-C8 a Cookie slot has `cookie = Some(_)`;
            // None here is a corrupted slot, treat as "no fingerprint".
            AuthMethod::Cookie => slot
                .cookie
                .as_ref()
                .map(|c| Self::cookie_prefix(c.as_ref())),
            AuthMethod::OAuth => slot
                .token
                .as_ref()
                .map(|t| Self::oauth_prefix(&t.refresh_token)),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct AccountPoolStatus {
    pub valid: Vec<AccountSlot>,
    pub exhausted: Vec<AccountSlot>,
    pub invalid: Vec<InvalidAccountSlot>,
}

/// Runtime state fields an in-flight request can write back to the pool.
/// Identical to [`RuntimeStateParams`] (the DB upsert payload) by design:
/// `release(account_id, update, reason)` funnels through the same fields
/// `apply_runtime_state` already consumes on the DB-load path. Carries no
/// credential bytes — credentials follow the "DB is authoritative" rule
/// and flow through `UpdateCredential` / reload merge, not release.
pub type RuntimeUpdate = RuntimeStateParams;

#[derive(Debug, Clone, Copy)]
enum RuntimeMergeMode {
    Full,
    OAuthSnapshot,
}

#[derive(Debug)]
enum AccountPoolMessage {
    /// Return an account with an id-keyed runtime update. The pool's own
    /// in-memory slot is the one that moves between buckets and keeps the
    /// authoritative credential — callers never ship a full `AccountSlot`.
    /// `update` is boxed because `RuntimeUpdate` carries 5 usage buckets
    /// and would otherwise dominate the enum layout.
    ///
    /// `expected_fingerprint` (Step 4 / C5) is the credential identity the
    /// caller saw at request-acquire time. `collect_by_id` compares it
    /// against the pool's current credential and discards stale releases
    /// — i.e., requests whose credential was admin-rotated mid-flight no
    /// longer poison the new credential's runtime / Reason. None means
    /// "no fingerprint available, skip the guard" (legacy / probe paths
    /// that still need wiring through C6).
    Return {
        account_id: i64,
        update: Box<RuntimeUpdate>,
        reason: Option<Reason>,
        expected_fingerprint: Option<CredentialFingerprint>,
        merge_mode: RuntimeMergeMode,
    },
    CheckReset,
    Request(
        Option<u64>,
        Vec<i64>,
        RpcReplyPort<Result<AccountSlot, ClewdrError>>,
    ),
    GetStatus(RpcReplyPort<AccountPoolStatus>),
    ReloadFromDb,
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
    /// Return a cheap in-memory pool snapshot for the health read path,
    /// along with the actor's DB handle. The caller runs
    /// `load_all_accounts` and `account_health::compose_health_snapshot`
    /// off-actor, so the `/health` / overview / accounts list endpoints
    /// do not serialise with dispatch / return traffic on this actor.
    /// See `docs/account-normalization-2026-04-21.md` §Step 2.5.
    SnapshotPoolState(RpcReplyPort<(PoolSnapshotView, SqlitePool)>),
}

#[derive(Debug)]
struct AccountPoolState {
    valid: VecDeque<AccountSlot>,
    exhausted: HashMap<i64, AccountSlot>,
    invalid: HashMap<i64, InvalidAccountSlot>,
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
        // Pull the account out of valid / exhausted, capturing its auth_method
        // along the way so the InvalidAccountSlot record can preserve the
        // kind for admin overview's invalid-grouping display. If the
        // account isn't in either bucket (already invalid, or never loaded)
        // we leave `state.invalid` untouched — over-writing it would erase
        // the existing reason without us being sure of the kind.
        let mut removed_kind: Option<AuthMethod> = None;
        state.valid.retain(|c| {
            if c.account_id == Some(account_id) {
                if removed_kind.is_none() {
                    removed_kind = Some(c.auth_method);
                }
                false
            } else {
                true
            }
        });

        if let Some(slot) = state.exhausted.remove(&account_id) {
            removed_kind.get_or_insert(slot.auth_method);
        }

        // Record in invalid so pool-view summaries and collect's sticky-reason
        // guard see the authoritative reason. Existing entry (if any) is
        // replaced so the reason reflects the latest cause.
        if let Some(auth_method) = removed_kind {
            state.invalid.insert(
                account_id,
                InvalidAccountSlot::new(account_id, auth_method, reason),
            );
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
        for uc in state.invalid.values() {
            state.dirty.insert(uc.account_id);
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

    fn log_account_summary(summary: &AccountHealthSummary) {
        let pool = &summary.pool;
        let detail = &summary.detail;
        info!(
            "Valid: {}, Exhausted: {}, Invalid: {} | Dispatchable: {}, Saturated: {}, Cooling: {}, Probing: {}, InvalidAuth: {}, InvalidDisabled: {}, Unconfigured: {}",
            pool.valid.to_string().green(),
            pool.exhausted.to_string().yellow(),
            pool.invalid.to_string().red(),
            detail.dispatchable_now.to_string().green(),
            detail.saturated.to_string().yellow(),
            detail.cooling_down.to_string().yellow(),
            detail.probing.to_string().cyan(),
            detail.invalid_auth.to_string().red(),
            detail.invalid_disabled.to_string().red(),
            detail.unconfigured.to_string().bright_black(),
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

    /// Account-id-keyed collect. Finds the pool's own slot for this
    /// `account_id`, merges `update` onto it, then moves it between
    /// `valid` / `exhausted` / `invalid` according to `reason`. Credential
    /// bytes on the pool's slot are never touched — only the runtime
    /// fields in `update`. See
    /// `docs/account-normalization-2026-04-21.md` §Step 3 Goal 1.
    /// Compute the credential fingerprint of the pool's *current* slot for
    /// `account_id`, by peeking each bucket without consuming. C5's guard
    /// compares this against the caller's request-time fingerprint.
    ///
    /// Lookup order: `valid` → `exhausted`. The `invalid` bucket no
    /// longer carries credential bytes (Step 4 / C6 retired
    /// `InvalidAccountSlot.cookie`) — we return None for invalid-only
    /// accounts. That's correct because every reason that can land an
    /// account in `invalid` (Free / Disabled / Banned / Null) is already
    /// caught by the sticky-reason guard above before this fingerprint
    /// check runs, so a None return here cannot mask a stale-write race.
    fn pool_credential_fingerprint(
        state: &AccountPoolState,
        account_id: i64,
    ) -> Option<CredentialFingerprint> {
        if let Some(slot) = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(account_id))
        {
            return CredentialFingerprint::from_slot(slot);
        }
        if let Some(slot) = state.exhausted.get(&account_id) {
            return CredentialFingerprint::from_slot(slot);
        }
        None
    }

    fn collect_by_id(
        state: &mut AccountPoolState,
        account_id: i64,
        update: RuntimeUpdate,
        reason: Option<Reason>,
        expected_fingerprint: Option<CredentialFingerprint>,
        merge_mode: RuntimeMergeMode,
    ) -> bool {
        let removed_probe = state.probing.remove(&account_id);

        // Sticky-reason guard: must peek `invalid` BEFORE we remove, so a
        // Return from an in-flight request whose account was explicitly
        // invalidated (auth_error / disabled / banned / free / null)
        // doesn't auto-reactivate. TMR / Restricted stay transient — they
        // intentionally flow through the cooldown reactivation path below.
        if let Some(existing) = state.invalid.get(&account_id)
            && matches!(
                existing.reason,
                Reason::Free | Reason::Disabled | Reason::Banned | Reason::Null
            )
        {
            return removed_probe;
        }

        // Fingerprint guard (Step 4 / C5): the caller captured the
        // credential identity at request-acquire time. If the pool's
        // current credential differs (admin reconnect or kind flip while
        // the request was in flight), the runtime + Reason in this update
        // belong to a credential that no longer represents this account
        // — applying either would either erase the new credential's
        // usage state or push a stale auth_error onto a healthy slot.
        //
        // OAuth refresh is *not* a mismatch: the fingerprint is the
        // refresh_token prefix, which survives a normal refresh.
        if let Some(expected) = expected_fingerprint.as_ref() {
            let actual = Self::pool_credential_fingerprint(state, account_id);
            if actual.as_ref() != Some(expected) {
                warn!(
                    "[release_runtime] credential fingerprint mismatch for account {account_id} \
                     (expected {:?}, actual {:?}); dropping stale runtime + reason",
                    expected, actual
                );
                return removed_probe;
            }
        }

        let had_valid = state
            .valid
            .iter()
            .position(|c| c.account_id == Some(account_id))
            .and_then(|i| state.valid.remove(i));
        let had_exhausted = state.exhausted.remove(&account_id);
        // Don't pop the invalid entry yet — if the account is *only* in
        // invalid we can't rebucket it (post-C6 invalid no longer carries
        // credential bytes for slot rebuild), so we'd otherwise leak it
        // out of every bucket. Only consume the entry if we actually have
        // a slot to migrate.
        let had_invalid_flag = state.invalid.contains_key(&account_id);

        let had_valid_flag = had_valid.is_some();
        let had_exhausted_flag = had_exhausted.is_some();

        // Prefer a full slot from valid / exhausted because it carries the
        // live credential. After Step 4 / C6 the `invalid` bucket no longer
        // stores credential bytes, so an account that's *only* in invalid
        // cannot be re-bucketed here — it stays put until `do_reload`
        // rebuilds it from DB. In practice every reason that lands an
        // account in `invalid` (Free / Disabled / Banned / Null) is
        // sticky and would have been caught by the sticky-reason guard
        // above, so this `_` arm is unreachable on the hot path; we
        // return defensively rather than panicking.
        let mut slot = match (had_valid, had_exhausted) {
            (Some(s), _) => s,
            (None, Some(s)) => s,
            _ => return removed_probe,
        };

        // We have a slot to rebucket — pop the invalid entry now (if any)
        // so the rebucket below is the sole writer for this account_id.
        let _ = state.invalid.remove(&account_id);

        match merge_mode {
            RuntimeMergeMode::Full => slot.apply_runtime_state(&update),
            RuntimeMergeMode::OAuthSnapshot => slot.apply_oauth_snapshot_runtime(&update),
        }

        let changed_set = match &reason {
            None => {
                if slot.reset_time.is_some() {
                    state.exhausted.insert(account_id, slot);
                    !had_exhausted_flag
                } else {
                    state.valid.push_back(slot);
                    !had_valid_flag
                }
            }
            Some(Reason::TooManyRequest(i) | Reason::Restricted(i)) => {
                slot.reset_time = Some(*i);
                slot.reset_window_usage();
                state.exhausted.insert(account_id, slot);
                !had_exhausted_flag
            }
            Some(reason) => {
                slot.reset_window_usage();
                state.invalid.insert(
                    account_id,
                    InvalidAccountSlot::new(account_id, slot.auth_method, reason.clone()),
                );
                !had_invalid_flag
            }
        };

        let moved_out_of_invalid = had_invalid_flag
            && matches!(
                &reason,
                None | Some(Reason::TooManyRequest(_) | Reason::Restricted(_))
            );
        if moved_out_of_invalid {
            state.reactivated.insert(account_id);
        }

        Self::mark_dirty(state, Some(account_id));
        if changed_set {
            Self::log(state);
        }
        removed_probe
    }

    /// Spawn a probe for `account_id`. The actor stays sync — DB load and
    /// auth-method dispatch happen off-actor in the spawned task. This is
    /// the unified probe entry point post Step 4 / C4: cookie accounts go
    /// through `probe_cookie`, OAuth accounts go through `probe_oauth_account`,
    /// and the routing is decided by the row's `auth_source` column at probe
    /// time (not by the cached slot's shape, not by a placeholder cookie).
    fn spawn_probe_guarded(
        state: &mut AccountPoolState,
        account_id: i64,
        log_sink: Option<broadcast::Sender<AdminEvent>>,
    ) {
        if state.probing.contains(&account_id) {
            return;
        }
        let Some(ref handle) = state.handle else {
            return;
        };
        // Snapshot the in-memory slot's runtime BEFORE we hand off to the
        // spawned task, so cookie probes pick up state that's newer than
        // the last `do_flush` (15s flush interval window). Without this,
        // an admin probe started in that window would rebuild the slot
        // from DB, miss any usage / count_tokens_allowed mutations the
        // last flush hasn't seen, and `probe_cookie`'s closing
        // `release_account` would write those stale values back over the
        // live runtime. OAuth probes already operate on the DB row
        // directly via `probe_oauth_account`, so this only matters for
        // the cookie branch.
        let mem_runtime: Option<RuntimeUpdate> = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(account_id))
            .map(|c| c.to_runtime_params())
            .or_else(|| {
                state
                    .exhausted
                    .get(&account_id)
                    .map(|c| c.to_runtime_params())
            });
        state.probing.insert(account_id);
        state.probe_errors.remove(&account_id);
        Self::emit_accounts_refresh(state);
        let handle = handle.clone();
        let db = state.db.clone();
        let profile = stealth::global_profile().clone();
        tokio::spawn(async move {
            // DB-load is authoritative for credential bytes (docs §"reload merge
            // 语义" #5). Without it we'd be re-hydrating from in-memory slot
            // residue, which is exactly what Step 4 retires.
            let account = match get_account_by_id(&db, account_id).await {
                Ok(Some(acc)) => acc,
                Ok(None) => {
                    let msg = format!("account {account_id} not found at probe time");
                    warn!("[probe] {msg}");
                    handle.set_probe_error(account_id, msg).await;
                    let _ = handle.clear_probing(account_id).await;
                    return;
                }
                Err(e) => {
                    let msg = format!("DB load failed: {e}");
                    warn!("[probe] account {account_id}: {msg}");
                    handle.set_probe_error(account_id, msg).await;
                    let _ = handle.clear_probing(account_id).await;
                    return;
                }
            };

            match AuthMethod::from_auth_source(&account.auth_source) {
                AuthMethod::OAuth => {
                    probe_oauth_account(account, handle, db, log_sink).await;
                }
                AuthMethod::Cookie => {
                    let mut slot =
                        match Self::build_cookie_probe_slot(&account, mem_runtime.as_ref()) {
                            Ok(s) => s,
                            Err(msg) => {
                                warn!("[probe] account {account_id}: {msg}");
                                handle.set_probe_error(account_id, msg).await;
                                let _ = handle.clear_probing(account_id).await;
                                return;
                            }
                        };
                    if let Some(token) = account.oauth_token.clone() {
                        slot.token = Some(token);
                    }
                    probe_cookie(account_id, slot, handle, profile, db, log_sink).await;
                }
            }
        });
    }

    /// Reconstruct an `AccountSlot` for a cookie account from its DB row,
    /// preserving the runtime state that the probe should release on
    /// completion. Returns the human-readable error message to surface
    /// via `set_probe_error` on failure.
    ///
    /// Runtime priority (highest first):
    ///   1. `mem_runtime` — caller-supplied snapshot from the pool's
    ///      current in-memory slot. Captured at probe-spawn time so we
    ///      include any usage / count_tokens_allowed mutations the next
    ///      `do_flush` hasn't yet persisted (15s flush window).
    ///   2. `account.runtime` — last-flushed runtime from the DB row.
    ///      Used for invalid-bucket probes (no in-memory slot exists),
    ///      and as fallback when `mem_runtime` is None.
    ///
    /// Without this back-fill, `probe_cookie`'s closing `release_account`
    /// would write defaults (`reset_time = None`,
    /// `count_tokens_allowed = None`, empty usage buckets, …) over the
    /// pool's live runtime — which would reset usage counters on every
    /// probe and demote exhausted accounts to valid on non-fatal
    /// usage-fetch failures.
    fn build_cookie_probe_slot(
        account: &AccountWithRuntime,
        mem_runtime: Option<&RuntimeUpdate>,
    ) -> Result<AccountSlot, String> {
        let cookie_blob = account
            .cookie_blob
            .as_deref()
            .ok_or_else(|| "cookie account missing cookie_blob".to_string())?;
        let mut slot =
            AccountSlot::new(cookie_blob, None).map_err(|e| format!("invalid cookie blob: {e}"))?;
        slot.account_id = Some(account.id);
        slot.auth_method = AuthMethod::Cookie;
        slot.proxy_url = account.proxy_url.clone();
        slot.email = account.email.clone();
        slot.account_type = account.account_type.clone();
        if let Some(params) = mem_runtime {
            slot.apply_runtime_state(params);
        } else if let Some(ref runtime) = account.runtime {
            slot.apply_runtime_state(&runtime.to_params());
        }
        // Normalize the reset boundary the same way do_reload does: lapsed
        // timestamps drop to None so the probe doesn't release an account
        // back into the exhausted bucket on a stale cooldown.
        //
        // We deliberately re-derive from the DB row's runtime, not from
        // mem_runtime: if mem holds a reset_time that's newer (freshly
        // observed cooldown) it'll already match active_reset_time
        // (writes to the DB row are routed via the same flush path).
        slot.reset_time = active_reset_time(account);
        Ok(slot)
    }

    /// Bootstrap auto-probe: fired after a reload completes. Fills missing
    /// metadata (`email`/`account_type`) for cookie accounts. OAuth accounts
    /// are intentionally skipped here — their token has already been
    /// validated by the OAuth grant flow, so a cookie-style probe adds
    /// nothing. Admin-triggered probes still cover OAuth via the unified
    /// dispatch in `spawn_probe_guarded`.
    fn spawn_probes_for_unprobed(state: &mut AccountPoolState) {
        let unprobed = Self::bootstrap_probe_account_ids(state);
        for account_id in unprobed {
            Self::spawn_probe_guarded(state, account_id, None);
        }
    }

    /// Account IDs eligible for the bootstrap auto-probe. Extracted so the
    /// (auth_method == Cookie) ∧ (missing metadata) filter is unit-testable
    /// without standing up a real actor / spawning real probe tasks.
    fn bootstrap_probe_account_ids(state: &AccountPoolState) -> Vec<i64> {
        state
            .valid
            .iter()
            .filter(|c| c.auth_method == AuthMethod::Cookie)
            .filter(|c| c.email.is_none() || c.account_type.is_none())
            .filter_map(|c| c.account_id)
            .collect()
    }

    /// Probe a caller-specified subset of accounts. Used by admin
    /// `POST /accounts/probe` (which now delegates the cookie/oauth split
    /// to `spawn_probe_guarded` instead of pre-routing OAuth itself) and
    /// per-account admin probes.
    ///
    /// Does NOT filter the wanted IDs against current pool buckets — the
    /// caller (admin) has already validated eligibility against the DB,
    /// and dropping unknown IDs here would silently lose freshly created
    /// accounts whose `reload_from_db()` cast hasn't been processed yet.
    /// `spawn_probe_guarded` re-validates via DB-load and surfaces
    /// "account not found" as a `set_probe_error`, so dispatching unknown
    /// IDs is safe.
    fn spawn_probe_accounts(
        state: &mut AccountPoolState,
        account_ids: &[i64],
        log_sink: Option<broadcast::Sender<AdminEvent>>,
    ) {
        for &account_id in account_ids {
            Self::spawn_probe_guarded(state, account_id, log_sink.clone());
        }
    }

    fn report(state: &AccountPoolState) -> AccountPoolStatus {
        AccountPoolStatus {
            valid: state.valid.clone().into(),
            exhausted: state.exhausted.values().cloned().collect(),
            invalid: state.invalid.values().cloned().collect(),
        }
    }

    /// Cheap in-memory snapshot of the pool fields needed by the health
    /// read path. Runs in a single actor turn with no DB I/O, so
    /// `/health` / admin overview / admin accounts list / reload log
    /// cannot head-of-line-block real dispatch traffic on the actor.
    /// Callers assemble the final `AccountHealthSnapshot` off-actor via
    /// `account_health::compose_health_snapshot`.
    fn snapshot_view(state: &AccountPoolState) -> PoolSnapshotView {
        PoolSnapshotView {
            valid_ids: state
                .valid
                .iter()
                .filter_map(|slot| slot.account_id)
                .collect(),
            exhausted: state
                .exhausted
                .iter()
                .map(|(id, slot)| (*id, slot.reset_time))
                .collect(),
            invalid: state
                .invalid
                .iter()
                .map(|(id, inv)| (*id, inv.reason.clone()))
                .collect(),
            inflight: state.inflight.clone(),
            probing: state.probing.clone(),
            probe_errors: state.probe_errors.clone(),
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
        for uc in state.invalid.values() {
            if dirty_ids.contains(&uc.account_id) {
                disabled.push((uc.account_id, uc.reason.to_db_string()));
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
                // Post Step 4 / C6 the invalid bucket only stores
                // (account_id, auth_method, reason) — no credential bytes.
                // Skip rows with no credential at all so we don't surface
                // a phantom invalid entry for a half-deleted account.
                if row.cookie_blob.is_none() && row.oauth_token.is_none() {
                    continue;
                }
                let reason = row
                    .invalid_reason
                    .as_deref()
                    .map(Reason::from_db_string)
                    .unwrap_or(Reason::Null);
                let auth_method = AuthMethod::from_auth_source(&row.auth_source);
                state
                    .invalid
                    .insert(row.id, InvalidAccountSlot::new(row.id, auth_method, reason));
                continue;
            }

            // Build the slot from whichever credential the row carries.
            // Step 4 / C8 onward: OAuth-only rows go through
            // `AccountSlot::oauth(...)` directly (no placeholder-cookie
            // synthesis); cookie rows continue through `AccountSlot::new`
            // which parses the blob. The common tail below stamps the
            // remaining row metadata onto either kind.
            let mut cs = match (row.cookie_blob.as_deref(), row.oauth_token.as_ref()) {
                (Some(cookie_str), _) => match AccountSlot::new(cookie_str, None) {
                    Ok(cs) => cs,
                    Err(e) => {
                        warn!("Invalid cookie for account '{}': {e}", row.name);
                        continue;
                    }
                },
                (None, Some(token)) => AccountSlot::oauth(row.id, token.clone()),
                (None, None) => continue,
            };
            cs.account_id = Some(row.id);
            cs.auth_method = AuthMethod::from_auth_source(&row.auth_source);
            cs.proxy_url = row.proxy_url.clone();
            cs.email = row.email.clone();
            cs.account_type = row.account_type.clone();
            if let Some(token) = row.oauth_token.clone() {
                cs.token = Some(token);
            }

            // Merge by credential-kind tuple, not cookie byte equality. Kind
            // flip (cookie↔oauth) = real credential replacement → fresh
            // defaults + probing cleanup. Same kind preserves runtime; DB
            // credential is authoritative and was already applied above when
            // `row.oauth_token` was attached to `cs`.
            //
            // `mem_kind` and `row_kind` both come from explicit AuthMethod
            // (Step 4 PR #6 / C3): mem reads its own field (loader stamps
            // it from row.auth_source on load); row reads `auth_source`
            // directly. This replaces the pre-C3 placeholder-cookie marker
            // and `row.oauth_token.is_some()` proxies — cookie accounts
            // hold a bearer token in `slot.token` after `exchange_token`,
            // so token presence is not a reliable kind discriminator.
            //
            // Within the cookie kind, a byte-level `cookie_blob` change is
            // treated as admin-initiated replacement (DB never changes
            // cookie bytes implicitly). OAuth access_token rotation from a
            // normal refresh is preserved — runtime/probing must survive.
            if let Some(mem) = mem_cookies.remove(&row.id) {
                let mem_kind = mem.auth_method;
                let row_kind = AuthMethod::from_auth_source(&row.auth_source);
                let same_kind = mem_kind == row_kind;
                let cookie_content_swap =
                    same_kind && row_kind == AuthMethod::Cookie && mem.cookie != cs.cookie;
                if same_kind && !cookie_content_swap {
                    Self::apply_in_memory_runtime(&mut cs, mem, row_kind == AuthMethod::Cookie);
                    cs.proxy_url = row.proxy_url.clone();
                } else {
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

        let view = Self::snapshot_view(state);
        let snapshot = compose_health_snapshot(&view, &accounts, Utc::now().timestamp());
        Self::log_account_summary(&snapshot.summary);

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
            invalid: HashMap::new(),
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
            AccountPoolMessage::Return {
                account_id,
                update,
                reason,
                expected_fingerprint,
                merge_mode,
            } => {
                let completed_probe = Self::collect_by_id(
                    state,
                    account_id,
                    *update,
                    reason,
                    expected_fingerprint,
                    merge_mode,
                );
                if completed_probe {
                    Self::do_flush(state).await;
                    Self::emit_accounts_refresh(state);
                }
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
            AccountPoolMessage::SnapshotPoolState(reply_port) => {
                reply_port.send((Self::snapshot_view(state), state.db.clone()))?;
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
            if let Some(id) = cs.account_id
                && dirty_ids.contains(&id)
            {
                params.push((id, cs.to_runtime_params()));
            }
        }
        if let Err(e) = batch_upsert_runtime_states(&state.db, &params).await {
            error!("Failed to flush runtime states on shutdown: {e}");
        }
        for uc in state.invalid.values() {
            if dirty_ids.contains(&uc.account_id)
                && let Err(e) =
                    set_account_disabled(&state.db, uc.account_id, &uc.reason.to_db_string()).await
            {
                error!(
                    "Failed to set account {} disabled on shutdown: {e}",
                    uc.account_id
                );
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

    /// Return an account to the pool with an id-keyed runtime update.
    /// The pool's own in-memory slot stays the source of truth for the
    /// account's credential — `update` only carries runtime-state fields
    /// (usage, utilization, reset_time, count_tokens_allowed, etc.).
    ///
    /// `expected_fingerprint` (Step 4 / C5): the credential identity the
    /// caller saw at request-acquire time. If the pool's slot has rotated
    /// since (admin replacement), the runtime update and Reason are both
    /// discarded — applying them would either reset the new credential's
    /// usage state or poison a healthy credential with a stale auth_error
    /// from the prior one. Pass `None` only when no slot context is
    /// available (e.g. probe paths that still rebuild from DB rows; those
    /// land via C6).
    pub async fn release_runtime(
        &self,
        account_id: i64,
        update: RuntimeUpdate,
        reason: Option<Reason>,
        expected_fingerprint: Option<CredentialFingerprint>,
    ) -> Result<(), ClewdrError> {
        ractor::cast!(
            self.actor_ref,
            AccountPoolMessage::Return {
                account_id,
                update: Box::new(update),
                reason,
                expected_fingerprint,
                merge_mode: RuntimeMergeMode::Full,
            }
        )
        .map_err(|e| ClewdrError::RactorError {
            loc: Location::generate(),
            msg: format!(
                "Failed to communicate with AccountPoolActor for release_runtime operation: {e}"
            ),
        })
    }

    /// Return OAuth profile/usage snapshot fields without clobbering local
    /// counters or capability probes in the pool slot.
    pub async fn release_oauth_snapshot_runtime(
        &self,
        account_id: i64,
        update: RuntimeUpdate,
        expected_fingerprint: Option<CredentialFingerprint>,
    ) -> Result<(), ClewdrError> {
        ractor::cast!(
            self.actor_ref,
            AccountPoolMessage::Return {
                account_id,
                update: Box::new(update),
                reason: None,
                expected_fingerprint,
                merge_mode: RuntimeMergeMode::OAuthSnapshot,
            }
        )
        .map_err(|e| ClewdrError::RactorError {
            loc: Location::generate(),
            msg: format!(
                "Failed to communicate with AccountPoolActor for release_oauth_snapshot_runtime operation: {e}"
            ),
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

    /// Fetch the unified account-health snapshot. Joins DB rows with the
    /// in-memory pool state inside the actor, so counts and per-account
    /// views are internally consistent.
    pub async fn get_health_snapshot(&self) -> Result<AccountHealthSnapshot, ClewdrError> {
        let (view, db) = ractor::call!(self.actor_ref, AccountPoolMessage::SnapshotPoolState)
            .map_err(|e| ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with AccountPoolActor for get_health_snapshot: {e}"
                ),
            })?;
        let accounts = load_all_accounts(&db).await?;
        let now = Utc::now().timestamp();
        Ok(compose_health_snapshot(&view, &accounts, now))
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
    use super::{AccountPoolActor, AccountPoolState, CredentialFingerprint, RuntimeMergeMode};
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::time::Duration;

    use moka::sync::Cache;
    use tokio::sync::broadcast;

    use crate::db::accounts::load_all_accounts;
    use crate::services::account_health::compose_health_snapshot;

    use crate::config::{AccountSlot, AuthMethod, Reason, TokenInfo};
    use crate::db::init_pool;

    #[test]
    fn in_memory_runtime_merge_keeps_db_oauth_token_when_present() {
        let mut reloaded = AccountSlot::oauth(
            7,
            TokenInfo::from_parts(
                "db-access".to_string(),
                "db-refresh".to_string(),
                Duration::from_secs(3600),
                "org-db".to_string(),
            ),
        );

        let mut mem = AccountSlot::oauth(
            7,
            TokenInfo::from_parts(
                "mem-access".to_string(),
                "mem-refresh".to_string(),
                Duration::from_secs(3600),
                "org-mem".to_string(),
            ),
        );
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
            invalid: HashMap::new(),
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
        let slot = oauth_slot_with_refresh(1, "rt0");
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
        let slot = oauth_slot_with_refresh(1, "rt0");
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
        let slot = oauth_slot_with_refresh(id, &format!("rt-{id}"));
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

        let slot = oauth_slot_with_refresh(1, "rt-1");
        // Account is sitting in `invalid` with a sticky reason (auth_error
        // reloaded → Reason::Null).
        state.invalid.insert(
            1,
            crate::config::InvalidAccountSlot::new(1, AuthMethod::Cookie, Reason::Null),
        );

        // In-flight request returns successfully (reason=None) — the pre-fix
        // behaviour would take from invalid and push back into valid, then
        // mark `state.reactivated` which drives `set_accounts_active` in
        // do_flush, clobbering the DB auth_error.
        AccountPoolActor::collect_by_id(
            &mut state,
            1,
            slot.to_runtime_params(),
            None,
            None,
            RuntimeMergeMode::Full,
        );

        assert!(
            state.invalid.contains_key(&1),
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
    /// (TooManyRequest / Restricted) flowing through `collect_by_id` from
    /// the EXHAUSTED bucket auto-reactivate when a later Return arrives
    /// with reason=None and a cleared reset_time. This is the existing
    /// "account cooled down, back in service" flow.
    ///
    /// Pre-C6 this also covered the rare TMR-in-INVALID case (auto
    /// re-bucket from invalid via the `(None, None, Some(inv))` arm of
    /// the slot-lookup match). Post-C6 the invalid bucket no longer
    /// carries credential bytes, so a TMR account that somehow ends up
    /// only in invalid waits for `do_reload` to rebuild it from DB
    /// instead — the in-production path here is exhausted-bucket
    /// reactivation, which is what this test now exercises.
    #[tokio::test]
    async fn collect_still_reactivates_for_cooldown_reason() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);

        // Account is sitting in EXHAUSTED with a TMR reset_time (the
        // production representation of "cooled down").
        let mut slot = oauth_slot_with_refresh(2, "rt0");
        slot.reset_time = Some(1_700_000_000);
        state.exhausted.insert(2, slot.clone());

        // The release captures the caller's view: reset_time has elapsed,
        // dispatch's `reset()` cleared it, and the request finished
        // normally (reason=None).
        let mut update = slot.to_runtime_params();
        update.reset_time = None;
        AccountPoolActor::collect_by_id(&mut state, 2, update, None, None, RuntimeMergeMode::Full);

        assert!(
            state.valid.iter().any(|c| c.account_id == Some(2)),
            "exhausted account released with reset_time=None must reactivate to valid"
        );
        assert!(
            !state.exhausted.contains_key(&2),
            "slot must move out of exhausted"
        );
    }

    /// Post-C6 invariant: a TMR account that somehow ends up *only* in
    /// the invalid bucket no longer auto-reactivates via release_runtime
    /// (the previous `(None, None, Some(inv))` slot-rebuild branch is
    /// gone, since `InvalidAccountSlot` no longer carries credential
    /// bytes). It stays in invalid until `do_reload` rebuilds it from
    /// DB. The dispatcher never picks invalid accounts, so no chat
    /// release would arrive for one in production — but if a stale
    /// release does arrive, we must NOT silently lose the account.
    #[tokio::test]
    async fn collect_leaves_invalid_only_account_in_invalid_after_c6() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);

        let slot = oauth_slot_with_refresh(2, "rt0");
        state.invalid.insert(
            2,
            crate::config::InvalidAccountSlot::new(
                2,
                AuthMethod::OAuth,
                Reason::TooManyRequest(1_700_000_000),
            ),
        );

        AccountPoolActor::collect_by_id(
            &mut state,
            2,
            slot.to_runtime_params(),
            None,
            None,
            RuntimeMergeMode::Full,
        );

        assert!(
            !state.valid.iter().any(|c| c.account_id == Some(2)),
            "invalid-only account must not be silently re-bucketed without DB context"
        );
        assert!(
            state.invalid.contains_key(&2),
            "invalid-only account must remain in invalid until do_reload rebuilds from DB"
        );
        assert!(
            !state.reactivated.contains(&2),
            "no reactivation queued without an actual rebucket"
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

        // Simulate the post-cooldown reactivation flow: slot is back in
        // `valid`, `reactivated` queues `set_accounts_active`, `dirty`
        // queues a runtime flush. Pre-C6 this state was reachable via
        // `collect_by_id`'s now-retired (None, None, Some(inv)) arm; the
        // setup is now manual since the only invariant under test is
        // `converge_invalidate`'s ability to drop *whatever* pending
        // flush side-effects are queued for an account it just decided
        // to invalidate.
        let slot = oauth_slot_with_refresh(1, "rt-1");
        state.valid.push_back(slot);
        state.reactivated.insert(1);
        state.dirty.insert(1);

        // Explicit failure path: probe writes auth_error to DB, then converges
        // the pool. Both queued flush side-effects must be cleared.
        AccountPoolActor::converge_invalidate(&mut state, 1, Reason::Null);
        assert!(
            !state.reactivated.contains(&1),
            "reactivated must be cleared"
        );
        assert!(!state.dirty.contains(&1), "dirty must be cleared");
        assert!(!state.valid.iter().any(|c| c.account_id == Some(1)));
        assert!(state.invalid.contains_key(&1));

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
        let slot = oauth_slot_with_refresh(1, "rt0");
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

    async fn insert_account_row(
        pool: &sqlx::SqlitePool,
        id: i64,
        status: &str,
        auth_source: &str,
        access: Option<&str>,
        refresh: Option<&str>,
        invalid_reason: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source,
                oauth_access_token, oauth_refresh_token, oauth_expires_at,
                organization_uuid, invalid_reason, drain_first
            ) VALUES (?1, ?2, ?1, 5, ?3, ?4, ?5, ?6, '2030-01-01T00:00:00Z', 'org', ?7, 0)",
        )
        .bind(id)
        .bind(format!("acc-{id}"))
        .bind(status)
        .bind(auth_source)
        .bind(access)
        .bind(refresh)
        .bind(invalid_reason)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn set_runtime_reset(pool: &sqlx::SqlitePool, id: i64, reset_time: i64) {
        sqlx::query(
            "INSERT INTO account_runtime_state (account_id, reset_time) VALUES (?1, ?2)
             ON CONFLICT(account_id) DO UPDATE SET reset_time = excluded.reset_time",
        )
        .bind(id)
        .bind(reset_time)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Bug-1-style regression: the unified snapshot must classify every
    /// account coherently — the same health.state the admin list shows,
    /// the same detail counts `/health` and overview read, and the same
    /// `probing_ids`/`last_errors` the frontend consumes. A disabled
    /// account currently under probe must keep its `Invalid { Disabled }`
    /// base state while still appearing in `detail.probing` and
    /// `probe.probing_ids`.
    #[tokio::test]
    async fn build_health_snapshot_unifies_pool_and_db_views() {
        use crate::services::account_health::{AccountHealthState, InvalidKind, PoolCounts};

        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();

        // id=1: active, will be valid + inflight 0/5 (dispatchable_now).
        // id=2: active, will be valid + inflight 5/5 (saturated).
        // id=3: active, will be exhausted with pool_reset_time in the future
        //       (cooling_down).
        // id=4: disabled + banned + in state.invalid + overlaid with probing
        //       (invalid_disabled ∩ probing).
        insert_account_row(&pool, 1, "active", "oauth", Some("at1"), Some("rt1"), None).await;
        insert_account_row(&pool, 2, "active", "oauth", Some("at2"), Some("rt2"), None).await;
        insert_account_row(&pool, 3, "active", "oauth", Some("at3"), Some("rt3"), None).await;
        insert_account_row(
            &pool,
            4,
            "disabled",
            "oauth",
            Some("at4"),
            Some("rt4"),
            Some("banned"),
        )
        .await;

        let future = chrono::Utc::now().timestamp() + 600;
        set_runtime_reset(&pool, 3, future).await;

        let mut state = empty_state(pool);

        // Valid slots for 1 and 2.
        let slot1 = oauth_slot_with_refresh(1, "rt-1");
        state.valid.push_back(slot1);
        state.inflight.insert(1, (0, 5));

        let slot2 = oauth_slot_with_refresh(2, "rt-2");
        state.valid.push_back(slot2);
        state.inflight.insert(2, (5, 5));

        // Cooling slot in exhausted carries the future reset_time in memory.
        let mut slot3 = oauth_slot_with_refresh(3, "rt-3");
        slot3.reset_time = Some(future);
        state.exhausted.insert(3, slot3);

        // Invalid slot for 4 with Reason::Banned, overlaid with probing.
        state.invalid.insert(
            4,
            crate::config::InvalidAccountSlot::new(4, AuthMethod::OAuth, Reason::Banned),
        );
        state.probing.insert(4);
        state.probe_errors.insert(4, "transient".to_string());

        let accounts = load_all_accounts(&state.db).await.unwrap();
        let view = AccountPoolActor::snapshot_view(&state);
        let snapshot = compose_health_snapshot(&view, &accounts, chrono::Utc::now().timestamp());

        assert_eq!(snapshot.summary.total, 4);
        assert_eq!(
            snapshot.summary.pool,
            PoolCounts {
                valid: 2,
                exhausted: 1,
                invalid: 1,
            }
        );

        let detail = snapshot.summary.detail;
        assert_eq!(detail.dispatchable_now, 1, "id=1 is ready to dispatch");
        assert_eq!(detail.saturated, 1, "id=2 has inflight cur >= max");
        assert_eq!(detail.cooling_down, 1, "id=3 is cooling");
        assert_eq!(detail.probing, 1, "id=4 overlays probing on disabled");
        assert_eq!(detail.invalid_disabled, 1);
        assert_eq!(detail.invalid_auth, 0);
        assert_eq!(detail.unconfigured, 0);

        assert_eq!(snapshot.summary.invalid_breakdown.banned, 1);
        assert_eq!(snapshot.summary.invalid_breakdown.disabled, 0);

        assert_eq!(snapshot.summary.probe.probing_count, 1);
        assert_eq!(snapshot.summary.probe.probing_ids, vec![4]);
        assert_eq!(
            snapshot
                .summary
                .probe
                .last_errors
                .get(&4)
                .map(String::as_str),
            Some("transient")
        );

        // auth_sources counts the DB auth_source column for all rows.
        assert_eq!(snapshot.summary.auth_sources.oauth, 4);
        assert_eq!(snapshot.summary.auth_sources.cookie, 0);

        // Per-account: the probing overlay must not change the base state.
        let h4 = snapshot
            .per_account
            .get(&4)
            .expect("id=4 must be in per_account");
        assert!(h4.probing, "id=4 is actively probing");
        assert_eq!(h4.last_probe_error.as_deref(), Some("transient"));
        assert!(
            matches!(
                h4.state,
                AccountHealthState::Invalid {
                    kind: InvalidKind::Disabled,
                    reason: Some(Reason::Banned),
                }
            ),
            "base state must survive the probing overlay: {:?}",
            h4.state
        );

        // Cooling account carries the pool reset_time.
        let h3 = snapshot.per_account.get(&3).expect("id=3");
        assert_eq!(
            h3.state,
            AccountHealthState::CoolingDown { reset_time: future }
        );
        assert!(!h3.probing);

        // Active account with saturated inflight still reports Active as its
        // base state — saturation is a detail slice, not a state change.
        let h2 = snapshot.per_account.get(&2).expect("id=2");
        assert_eq!(h2.state, AccountHealthState::Active);
    }

    /// Regression: between `collect` / `reset` and the next `do_flush`, the
    /// pool's bucket and the DB row disagree. `build_health_snapshot` must
    /// trust the pool, otherwise the admin list and overview show stale
    /// CoolingDown/Active entries even though dispatch has already moved on.
    #[tokio::test]
    async fn build_health_snapshot_pool_bucket_overrides_stale_db() {
        use crate::services::account_health::{AccountHealthState, InvalidKind};

        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();

        // id=5: DB says active + runtime.reset_time still in the future,
        //       but the pool has already moved the slot back to `valid`
        //       (stale cooldown row). Expected: Active.
        // id=6: DB still says active, but the pool has just moved the
        //       account into `state.invalid` with Reason::Banned. Expected:
        //       Invalid { AuthError, Banned }.
        insert_account_row(&pool, 5, "active", "oauth", Some("at5"), Some("rt5"), None).await;
        insert_account_row(&pool, 6, "active", "oauth", Some("at6"), Some("rt6"), None).await;

        let future = chrono::Utc::now().timestamp() + 600;
        set_runtime_reset(&pool, 5, future).await;

        let mut state = empty_state(pool);

        let slot5 = oauth_slot_with_refresh(5, "rt-5");
        // Pool slot has no reset_time — the account just got reset()-ed.
        state.valid.push_back(slot5);
        state.inflight.insert(5, (0, 5));

        state.invalid.insert(
            6,
            crate::config::InvalidAccountSlot::new(6, AuthMethod::OAuth, Reason::Banned),
        );

        let accounts = load_all_accounts(&state.db).await.unwrap();
        let view = AccountPoolActor::snapshot_view(&state);
        let snapshot = compose_health_snapshot(&view, &accounts, chrono::Utc::now().timestamp());

        let h5 = snapshot.per_account.get(&5).expect("id=5");
        assert_eq!(
            h5.state,
            AccountHealthState::Active,
            "pool bucket Valid must beat stale DB reset_time"
        );

        let h6 = snapshot.per_account.get(&6).expect("id=6");
        assert!(
            matches!(
                h6.state,
                AccountHealthState::Invalid {
                    kind: InvalidKind::AuthError,
                    reason: Some(Reason::Banned),
                }
            ),
            "pool bucket Invalid must beat stale DB status=active: {:?}",
            h6.state
        );

        assert_eq!(snapshot.summary.detail.dispatchable_now, 1);
        assert_eq!(snapshot.summary.detail.cooling_down, 0);
        assert_eq!(snapshot.summary.detail.invalid_auth, 1);
    }

    /// Step 3 Goal 1 invariant: `collect_by_id` merges the runtime update
    /// onto the pool's own slot — the caller cannot overwrite credentials
    /// through release. OAuth refresh paths keep the DB authoritative and
    /// the pool's slot stays in sync via `update_credential`.
    #[tokio::test]
    async fn collect_by_id_preserves_pool_credential_over_caller_state() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);

        let slot = oauth_slot_with_refresh(77, "rt_authoritative");
        state.valid.push_back(slot.clone());
        state.inflight.insert(77, (0, 5));

        // Runtime update carries only runtime-state fields (flip a flag).
        let mut update = slot.to_runtime_params();
        update.count_tokens_allowed = Some(true);

        AccountPoolActor::collect_by_id(&mut state, 77, update, None, None, RuntimeMergeMode::Full);

        let after = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(77))
            .expect("slot must remain in valid");
        assert_eq!(after.count_tokens_allowed, Some(true));
        assert_eq!(
            after.token.as_ref().map(|t| t.refresh_token.as_str()),
            Some("rt_authoritative"),
            "pool credential must not be overwritten by release payload"
        );
    }

    /// Regression for codex finding 2026-04-24: cookie accounts exchange
    /// their cookie for a short-lived bearer token during
    /// `ClaudeCodeState::exchange_token`, so `mem.token.is_some()` is NOT
    /// a reliable OAuth-kind discriminator. If it were, a cookie account
    /// that had served any request would be misclassified on the next
    /// reload and its runtime / probing state reset.
    #[tokio::test]
    async fn reload_preserves_cookie_account_with_exchanged_bearer_token() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let cookie_blob = cookie_blob_for(b'c');
        insert_cookie_account_row(&pool, 50, &cookie_blob).await;

        let mut state = empty_state(pool);
        let mut mem_slot = AccountSlot::new(&cookie_blob, None).unwrap();
        mem_slot.account_id = Some(50);
        // Cookie account has exchanged its cookie for a bearer token —
        // this is normal after the first request.
        mem_slot.token = Some(token_with_refresh("cookie_exchanged_bearer"));
        mem_slot.count_tokens_allowed = Some(true);
        state.valid.push_back(mem_slot);
        state.probing.insert(50);

        AccountPoolActor::do_reload(&mut state).await;

        let slot = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(50))
            .expect("cookie account must survive reload");
        assert_eq!(
            slot.count_tokens_allowed,
            Some(true),
            "same-kind cookie reload must preserve runtime"
        );
        assert!(
            state.probing.contains(&50),
            "same-kind cookie reload must not clear probing"
        );
    }

    /// Within the cookie kind, a cookie_blob byte swap represents admin-
    /// initiated credential replacement (DB never changes cookie_blob
    /// implicitly). Runtime and probing state must reset.
    #[tokio::test]
    async fn reload_resets_on_cookie_content_swap() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let new_cookie = cookie_blob_for(b'd');
        insert_cookie_account_row(&pool, 51, &new_cookie).await;

        let mut state = empty_state(pool);
        let old_cookie = cookie_blob_for(b'e');
        let mut mem_slot = AccountSlot::new(&old_cookie, None).unwrap();
        mem_slot.account_id = Some(51);
        mem_slot.count_tokens_allowed = Some(true);
        state.valid.push_back(mem_slot);
        state.probing.insert(51);

        AccountPoolActor::do_reload(&mut state).await;

        let slot = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(51))
            .expect("reloaded slot must appear in valid");
        assert!(
            slot.count_tokens_allowed.is_none(),
            "cookie content swap must reset runtime"
        );
        assert!(
            !state.probing.contains(&51),
            "cookie content swap must clear probing"
        );
    }

    async fn insert_cookie_account_row(pool: &sqlx::SqlitePool, id: i64, cookie_blob: &str) {
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source, cookie_blob,
                organization_uuid, drain_first
            ) VALUES (?1, ?2, ?1, 5, 'active', 'cookie', ?3, 'org', 0)",
        )
        .bind(id)
        .bind(format!("acc-{id}"))
        .bind(cookie_blob)
        .execute(pool)
        .await
        .unwrap();
    }

    fn cookie_blob_for(seed: u8) -> String {
        // Shape matches ClewdrCookie's regex (sid01 = real session cookie).
        let body: String = std::iter::repeat_n(seed as char, 86).collect();
        format!("sk-ant-sid01-{body}-aaaaaaAA")
    }

    /// Regression for Step 3 Goal 3: a byte-level OAuth `access_token`
    /// change (the shape of a normal refresh) must NOT be treated as
    /// credential replacement by the reload merge. Runtime and probing
    /// state survive; DB credential bytes become authoritative.
    #[tokio::test]
    async fn reload_preserves_runtime_on_oauth_refresh() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        insert_account_row(
            &pool,
            42,
            "active",
            "oauth",
            Some("at_new"),
            Some("rt_new"),
            None,
        )
        .await;

        let mut state = empty_state(pool);
        let mut mem_slot = oauth_slot_with_refresh(42, "rt_stale");
        mem_slot.count_tokens_allowed = Some(true);
        mem_slot.supports_claude_1m_sonnet = Some(true);
        state.valid.push_back(mem_slot);
        state.inflight.insert(42, (0, 5));
        state.probing.insert(42);

        AccountPoolActor::do_reload(&mut state).await;

        let slot = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(42))
            .expect("same-kind reload must keep id=42 in valid");
        assert_eq!(
            slot.count_tokens_allowed,
            Some(true),
            "same-kind reload must preserve in-memory runtime"
        );
        assert_eq!(slot.supports_claude_1m_sonnet, Some(true));
        assert_eq!(
            slot.token.as_ref().map(|t| t.access_token.as_str()),
            Some("at_new"),
            "DB is authoritative for oauth credential bytes"
        );
        assert_eq!(
            slot.token.as_ref().map(|t| t.refresh_token.as_str()),
            Some("rt_new"),
        );
        assert!(
            state.probing.contains(&42),
            "same-kind reload must not clear probing state"
        );
    }

    /// Credential kind flip (OAuth → Cookie): user pasted a cookie,
    /// wiping the OAuth credential. Runtime defaults must be applied and
    /// probing state cleared.
    #[tokio::test]
    async fn reload_resets_on_kind_flip_oauth_to_cookie() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let cookie_blob = cookie_blob_for(b'a');
        insert_cookie_account_row(&pool, 43, &cookie_blob).await;

        let mut state = empty_state(pool);
        let mut mem_slot = oauth_slot_with_refresh(43, "rt_old");
        mem_slot.count_tokens_allowed = Some(true);
        state.valid.push_back(mem_slot);
        state.probing.insert(43);

        AccountPoolActor::do_reload(&mut state).await;

        let slot = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(43))
            .expect("id=43 must appear in reloaded valid");
        assert!(
            slot.count_tokens_allowed.is_none(),
            "kind flip must reset runtime to defaults"
        );
        assert!(
            slot.token.is_none(),
            "cookie account must not retain stale OAuth token"
        );
        assert!(
            !state.probing.contains(&43),
            "probing must be cleared on credential replacement"
        );
    }

    /// Credential kind flip (Cookie → OAuth): user switched auth method
    /// via admin API. Same semantics as above but the opposite direction.
    #[tokio::test]
    async fn reload_resets_on_kind_flip_cookie_to_oauth() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        insert_account_row(
            &pool,
            44,
            "active",
            "oauth",
            Some("at_fresh"),
            Some("rt_fresh"),
            None,
        )
        .await;

        let mut state = empty_state(pool);
        let cookie_blob = cookie_blob_for(b'b');
        let mut mem_slot = AccountSlot::new(&cookie_blob, None).unwrap();
        mem_slot.account_id = Some(44);
        mem_slot.count_tokens_allowed = Some(true);
        state.valid.push_back(mem_slot);
        state.probing.insert(44);

        AccountPoolActor::do_reload(&mut state).await;

        let slot = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(44))
            .expect("id=44 must appear in reloaded valid");
        assert!(
            slot.count_tokens_allowed.is_none(),
            "kind flip must reset runtime to defaults"
        );
        assert_eq!(
            slot.token.as_ref().map(|t| t.access_token.as_str()),
            Some("at_fresh"),
            "oauth token from DB must be attached on kind flip"
        );
        assert!(
            !state.probing.contains(&44),
            "probing must be cleared on credential replacement"
        );
    }

    /// Loader must stamp `auth_method` from `accounts.auth_source` so the
    /// rest of Step 4 can dispatch send-path / probe-path / reload-merge
    /// without reading cookie shape. Two rows of opposite kinds in the
    /// same reload prove the column is read per-row, not stuck on a
    /// process-wide constant.
    #[tokio::test]
    async fn reload_stamps_auth_method_from_row_auth_source() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let cookie_blob = cookie_blob_for(b'c');
        insert_cookie_account_row(&pool, 60, &cookie_blob).await;
        insert_account_row(
            &pool,
            61,
            "active",
            "oauth",
            Some("at_a"),
            Some("rt_a"),
            None,
        )
        .await;

        let mut state = empty_state(pool);
        AccountPoolActor::do_reload(&mut state).await;

        let cookie_slot = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(60))
            .expect("cookie account 60 must load");
        assert_eq!(
            cookie_slot.auth_method,
            AuthMethod::Cookie,
            "row auth_source='cookie' must stamp AuthMethod::Cookie"
        );

        let oauth_slot = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(61))
            .expect("oauth account 61 must load");
        assert_eq!(
            oauth_slot.auth_method,
            AuthMethod::OAuth,
            "row auth_source='oauth' must stamp AuthMethod::OAuth"
        );
    }

    /// Bootstrap auto-probe (`spawn_probes_for_unprobed`) is meant to fill
    /// missing `email` / `account_type` for cookie accounts. Post-C4 the
    /// filter is `auth_method == Cookie ∧ (email | account_type missing)`,
    /// replacing the pre-C4 `is_oauth_placeholder_slot` shape check. OAuth
    /// accounts must NOT be enumerated here — their token is already
    /// validated and a cookie-style probe would either fail or do nothing
    /// useful.
    #[tokio::test]
    async fn bootstrap_probe_skips_oauth_and_completed_cookie_slots() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);

        // Cookie account 1: missing email → should be probed
        let mut cookie_unprobed = AccountSlot::new(&cookie_blob_for(b'a'), None).unwrap();
        cookie_unprobed.account_id = Some(1);
        cookie_unprobed.auth_method = AuthMethod::Cookie;
        state.valid.push_back(cookie_unprobed);

        // Cookie account 2: full metadata → should NOT be probed
        let mut cookie_complete = AccountSlot::new(&cookie_blob_for(b'b'), None).unwrap();
        cookie_complete.account_id = Some(2);
        cookie_complete.auth_method = AuthMethod::Cookie;
        cookie_complete.email = Some("x@y".into());
        cookie_complete.account_type = Some("Pro".into());
        state.valid.push_back(cookie_complete);

        // OAuth account 3: missing email → STILL skipped (auth_method gate)
        let oauth_slot = oauth_slot_with_refresh(3, "rt-3");
        state.valid.push_back(oauth_slot);

        let ids = AccountPoolActor::bootstrap_probe_account_ids(&state);
        assert_eq!(ids, vec![1], "only the unprobed cookie account is eligible");
    }

    /// Post-C4 admin probe path enumerates every requested ID and lets
    /// `spawn_probe_guarded` validate each via DB-load. The pre-PR-7-fix
    /// enumeration filtered to "IDs already in pool buckets", which
    /// silently dropped freshly-created accounts whose `reload_from_db`
    /// cast hadn't been processed yet (admin create / reconnect /
    /// update → immediate /accounts/probe race). This test pins that
    /// `spawn_probe_accounts` no longer applies that filter.
    ///
    /// Verified indirectly via `state.probing` because the actor `handle`
    /// is None in `empty_state`, so `spawn_probe_guarded`'s sync prelude
    /// runs (which would insert into `probing`) but the spawned task is
    /// never created (early return on missing handle). When `handle` is
    /// None, `state.probing` stays empty too — so this test confirms the
    /// enumeration shape rather than the dispatch outcome.
    #[tokio::test]
    async fn spawn_probe_accounts_enumerates_every_requested_id_without_bucket_filter() {
        // We can't easily observe spawn_probe_guarded's effects without a
        // real actor handle, but we can confirm that the enumeration
        // helper itself doesn't drop unknown IDs. Since the production
        // path is now "for &id in account_ids: spawn_probe_guarded(id)",
        // the only thing to assert is that every input ID survives to
        // the dispatch call. Validate by treating spawn_probe_guarded as
        // a no-op (no handle) and checking state isn't mutated for
        // anything we shouldn't touch.
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);
        // No handle set — spawn_probe_guarded will early-return.
        let wanted = vec![10_i64, 11, 12, 999];
        AccountPoolActor::spawn_probe_accounts(&mut state, &wanted, None);
        // No panic, no state mutation. The real coverage of "IDs survive
        // to dispatch" lives in integration tests around admin
        // /accounts/probe (race-with-reload scenario).
        assert!(state.probing.is_empty());
        assert!(state.probe_errors.is_empty());
    }

    /// Regression for v3 review 2026-04-24: post-C4 the cookie probe slot
    /// is rebuilt from a DB row instead of inheriting the in-memory slot.
    /// Without runtime back-fill, `probe_cookie`'s closing
    /// `release_account(...)` would write default `reset_time = None` /
    /// `count_tokens_allowed = None` / etc. into the pool, which in turn
    /// demotes exhausted cookie accounts to valid on any non-fatal
    /// usage-fetch failure. `build_cookie_probe_slot` must apply
    /// `row.runtime` and normalize `reset_time` via `active_reset_time`.
    #[test]
    fn build_cookie_probe_slot_preserves_runtime_state_from_db_row() {
        use crate::db::accounts::{AccountWithRuntime, RuntimeStateRow};

        let future_reset = chrono::Utc::now().timestamp() + 3600;
        let runtime = RuntimeStateRow {
            reset_time: Some(future_reset),
            supports_claude_1m_sonnet: Some(false),
            supports_claude_1m_opus: Some(true),
            count_tokens_allowed: Some(true),
            session_resets_at: Some(future_reset + 100),
            weekly_resets_at: None,
            weekly_sonnet_resets_at: None,
            weekly_opus_resets_at: None,
            resets_last_checked_at: Some(future_reset - 50),
            session_has_reset: Some(true),
            weekly_has_reset: None,
            weekly_sonnet_has_reset: None,
            weekly_opus_has_reset: None,
            session_utilization: Some(0.42),
            weekly_utilization: None,
            weekly_sonnet_utilization: None,
            weekly_opus_utilization: None,
            buckets: Default::default(),
        };
        let account = AccountWithRuntime {
            id: 7,
            name: "acc-7".into(),
            rr_order: 7,
            max_slots: 5,
            proxy_id: None,
            proxy_name: None,
            proxy_url: Some("http://proxy".into()),
            drain_first: false,
            status: "active".into(),
            auth_source: "cookie".into(),
            cookie_blob: Some(cookie_blob_for(b'p')),
            oauth_token: None,
            oauth_expires_at: None,
            last_refresh_at: None,
            last_error: None,
            organization_uuid: None,
            invalid_reason: None,
            email: Some("u@e".into()),
            account_type: Some("Pro".into()),
            created_at: None,
            updated_at: None,
            runtime: Some(runtime),
        };

        let slot = AccountPoolActor::build_cookie_probe_slot(&account, None)
            .expect("cookie row must build a probe slot");

        assert_eq!(slot.account_id, Some(7));
        assert_eq!(slot.auth_method, AuthMethod::Cookie);
        assert_eq!(slot.proxy_url.as_deref(), Some("http://proxy"));
        assert_eq!(slot.email.as_deref(), Some("u@e"));
        assert_eq!(slot.account_type.as_deref(), Some("Pro"));

        assert_eq!(
            slot.reset_time,
            Some(future_reset),
            "exhausted cookie row's reset_time must propagate so probe doesn't \
             release with reset_time=None and demote the slot to valid"
        );
        assert_eq!(slot.count_tokens_allowed, Some(true));
        assert_eq!(slot.supports_claude_1m_sonnet, Some(false));
        assert_eq!(slot.supports_claude_1m_opus, Some(true));
        assert_eq!(slot.session_resets_at, Some(future_reset + 100));
        assert_eq!(slot.session_has_reset, Some(true));
        assert!((slot.session_utilization.unwrap() - 0.42).abs() < f64::EPSILON);
    }

    /// Companion regression: a runtime row whose `reset_time` already
    /// elapsed must be normalized to None — exactly what `do_reload`'s
    /// no-mem branch does. Otherwise the probe would treat the account as
    /// exhausted when the real cooldown has lifted.
    #[test]
    fn build_cookie_probe_slot_normalizes_lapsed_reset_time() {
        use crate::db::accounts::{AccountWithRuntime, RuntimeStateRow};

        let lapsed = chrono::Utc::now().timestamp() - 60;
        let runtime = RuntimeStateRow {
            reset_time: Some(lapsed),
            supports_claude_1m_sonnet: None,
            supports_claude_1m_opus: None,
            count_tokens_allowed: None,
            session_resets_at: None,
            weekly_resets_at: None,
            weekly_sonnet_resets_at: None,
            weekly_opus_resets_at: None,
            resets_last_checked_at: None,
            session_has_reset: None,
            weekly_has_reset: None,
            weekly_sonnet_has_reset: None,
            weekly_opus_has_reset: None,
            session_utilization: None,
            weekly_utilization: None,
            weekly_sonnet_utilization: None,
            weekly_opus_utilization: None,
            buckets: Default::default(),
        };
        let account = AccountWithRuntime {
            id: 8,
            name: "acc-8".into(),
            rr_order: 8,
            max_slots: 5,
            proxy_id: None,
            proxy_name: None,
            proxy_url: None,
            drain_first: false,
            status: "active".into(),
            auth_source: "cookie".into(),
            cookie_blob: Some(cookie_blob_for(b'q')),
            oauth_token: None,
            oauth_expires_at: None,
            last_refresh_at: None,
            last_error: None,
            organization_uuid: None,
            invalid_reason: None,
            email: None,
            account_type: None,
            created_at: None,
            updated_at: None,
            runtime: Some(runtime),
        };

        let slot = AccountPoolActor::build_cookie_probe_slot(&account, None).unwrap();
        assert!(
            slot.reset_time.is_none(),
            "lapsed reset_time must normalize to None"
        );
    }

    /// Cookie account row missing `cookie_blob` (data inconsistency) must
    /// fail closed. The error message becomes the `set_probe_error` payload
    /// so admins can diagnose the row state.
    #[test]
    fn build_cookie_probe_slot_rejects_missing_cookie_blob() {
        use crate::db::accounts::AccountWithRuntime;

        let account = AccountWithRuntime {
            id: 9,
            name: "acc-9".into(),
            rr_order: 9,
            max_slots: 5,
            proxy_id: None,
            proxy_name: None,
            proxy_url: None,
            drain_first: false,
            status: "active".into(),
            auth_source: "cookie".into(),
            cookie_blob: None,
            oauth_token: None,
            oauth_expires_at: None,
            last_refresh_at: None,
            last_error: None,
            organization_uuid: None,
            invalid_reason: None,
            email: None,
            account_type: None,
            created_at: None,
            updated_at: None,
            runtime: None,
        };

        let err = AccountPoolActor::build_cookie_probe_slot(&account, None).unwrap_err();
        assert!(
            err.contains("cookie_blob"),
            "error message must mention the missing field, got: {err}"
        );
    }

    /// Post-fix invariant: when the actor hands `spawn_probe_guarded` an
    /// in-memory runtime snapshot (captured from valid/exhausted before
    /// spawning), `build_cookie_probe_slot` MUST use it instead of the
    /// DB row's runtime. Otherwise an admin probe started inside the 15s
    /// flush window would re-write the live slot with stale usage /
    /// count_tokens_allowed on probe completion (probe_cookie's
    /// release_account returns the entire slot runtime).
    ///
    /// The `reset_time` comes from `active_reset_time(account)` regardless —
    /// it's derived from the DB runtime, but that's kept in sync via the
    /// same flush path, so it matches in practice.
    #[test]
    fn build_cookie_probe_slot_prefers_memory_runtime_over_db_row() {
        use crate::db::accounts::{AccountWithRuntime, RuntimeStateRow};

        // DB row's runtime: `count_tokens_allowed = false` (last flushed).
        let db_runtime = RuntimeStateRow {
            reset_time: None,
            supports_claude_1m_sonnet: Some(false),
            supports_claude_1m_opus: Some(false),
            count_tokens_allowed: Some(false),
            session_resets_at: None,
            weekly_resets_at: None,
            weekly_sonnet_resets_at: None,
            weekly_opus_resets_at: None,
            resets_last_checked_at: None,
            session_has_reset: None,
            weekly_has_reset: None,
            weekly_sonnet_has_reset: None,
            weekly_opus_has_reset: None,
            session_utilization: None,
            weekly_utilization: None,
            weekly_sonnet_utilization: None,
            weekly_opus_utilization: None,
            buckets: Default::default(),
        };
        let account = AccountWithRuntime {
            id: 20,
            name: "acc-20".into(),
            rr_order: 20,
            max_slots: 5,
            proxy_id: None,
            proxy_name: None,
            proxy_url: None,
            drain_first: false,
            status: "active".into(),
            auth_source: "cookie".into(),
            cookie_blob: Some(cookie_blob_for(b'r')),
            oauth_token: None,
            oauth_expires_at: None,
            last_refresh_at: None,
            last_error: None,
            organization_uuid: None,
            invalid_reason: None,
            email: None,
            account_type: None,
            created_at: None,
            updated_at: None,
            runtime: Some(db_runtime),
        };

        // In-memory snapshot: `count_tokens_allowed = true`, some session
        // usage — mutations made since the last flush.
        let mut mem_slot = AccountSlot::new(&cookie_blob_for(b'r'), None).unwrap();
        mem_slot.account_id = Some(20);
        mem_slot.auth_method = AuthMethod::Cookie;
        mem_slot.count_tokens_allowed = Some(true);
        mem_slot.supports_claude_1m_sonnet = Some(true);
        mem_slot.session_usage.total_input_tokens = 12345;
        let mem_runtime = mem_slot.to_runtime_params();

        let slot = AccountPoolActor::build_cookie_probe_slot(&account, Some(&mem_runtime)).unwrap();
        assert_eq!(
            slot.count_tokens_allowed,
            Some(true),
            "in-memory snapshot must win over DB row"
        );
        assert_eq!(slot.supports_claude_1m_sonnet, Some(true));
        assert_eq!(slot.session_usage.total_input_tokens, 12345);
    }

    fn cookie_slot_with_blob(account_id: i64, blob: &str) -> AccountSlot {
        let mut slot = AccountSlot::new(blob, None).unwrap();
        slot.account_id = Some(account_id);
        slot.auth_method = AuthMethod::Cookie;
        slot
    }

    fn oauth_slot_with_refresh(account_id: i64, refresh: &str) -> AccountSlot {
        AccountSlot::oauth(account_id, token_with_refresh(refresh))
    }

    /// C5 race scenario 1: a chat request acquired a cookie account, then
    /// admin reconnect rotated the credential to a brand-new cookie blob
    /// before the request finished. The release carries the OLD cookie's
    /// fingerprint and a `Reason::Null` (generic auth failure on the now-
    /// stale cookie). Without the fingerprint guard, `collect_by_id`
    /// would push the NEW slot into `invalid` with the stale auth error.
    #[tokio::test]
    async fn collect_by_id_drops_stale_release_after_admin_credential_swap() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);

        let original_blob = cookie_blob_for(b'a');
        let new_blob = cookie_blob_for(b'b');
        // Pool currently holds the NEW credential (post admin swap).
        state.valid.push_back(cookie_slot_with_blob(1, &new_blob));
        state.inflight.insert(1, (0, 5));

        // Caller captured fingerprint at acquire time, before the swap.
        let request_time_slot = cookie_slot_with_blob(1, &original_blob);
        let stale_fp = CredentialFingerprint::from_slot(&request_time_slot);
        assert!(stale_fp.is_some());

        let update = request_time_slot.to_runtime_params();
        AccountPoolActor::collect_by_id(
            &mut state,
            1,
            update,
            Some(Reason::Null),
            stale_fp,
            RuntimeMergeMode::Full,
        );

        // Pool slot must remain in valid, untouched. The stale auth_error
        // reason must NOT have demoted the new credential.
        assert_eq!(
            state.valid.len(),
            1,
            "new credential must stay in valid bucket"
        );
        assert!(
            !state.invalid.contains_key(&1),
            "stale Reason::Null must not push the rotated cookie into invalid"
        );
        let surviving_blob = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(1))
            .and_then(|c| c.cookie.as_ref().map(|cookie| cookie.to_string()))
            .unwrap();
        assert!(
            surviving_blob.contains(&new_blob[..20]),
            "new cookie blob must remain (got: {})",
            surviving_blob
        );
    }

    /// C5 race scenario 2: an OAuth refresh swapped the access_token but
    /// kept the refresh_token (a normal refresh, not an admin reconnect).
    /// The fingerprint is the refresh_token prefix, so the caller's
    /// capture from before the refresh must still match the pool's
    /// current slot. The runtime update MUST be applied — otherwise every
    /// request that overlaps a refresh would lose its usage / boundary
    /// updates.
    #[tokio::test]
    async fn collect_by_id_accepts_release_across_oauth_refresh_same_refresh_token() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);

        // Pool slot post-refresh: same refresh_token "rt_stable", new access_token "at_new".
        let mut pool_slot = oauth_slot_with_refresh(2, "rt_stable");
        pool_slot.token = Some(TokenInfo::from_parts(
            "at_new".into(),
            "rt_stable".into(),
            Duration::from_secs(3600),
            "org".into(),
        ));
        state.valid.push_back(pool_slot);
        state.inflight.insert(2, (0, 5));

        // Caller captured fingerprint before the refresh. access_token was
        // "at_old" then; refresh_token was the same "rt_stable".
        let mut request_time_slot = oauth_slot_with_refresh(2, "rt_stable");
        request_time_slot.token = Some(TokenInfo::from_parts(
            "at_old".into(),
            "rt_stable".into(),
            Duration::from_secs(3600),
            "org".into(),
        ));
        let fp = CredentialFingerprint::from_slot(&request_time_slot);
        assert!(matches!(fp, Some(CredentialFingerprint::OAuth(_))));

        // Bring an interesting runtime mutation through the release.
        let mut update = request_time_slot.to_runtime_params();
        update.count_tokens_allowed = Some(true);
        AccountPoolActor::collect_by_id(&mut state, 2, update, None, fp, RuntimeMergeMode::Full);

        let after = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(2))
            .expect("OAuth slot must remain in valid");
        assert_eq!(
            after.count_tokens_allowed,
            Some(true),
            "release across an OAuth refresh (refresh_token unchanged) must apply runtime"
        );
        assert_eq!(
            after.token.as_ref().map(|t| t.access_token.as_str()),
            Some("at_new"),
            "credential bytes are pool-owned; release_runtime must not touch them"
        );
    }

    #[tokio::test]
    async fn oauth_probe_runtime_release_updates_pool_before_next_flush() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        insert_oauth_row(&pool, 2, "at_probe", "rt_probe").await;
        let mut state = empty_state(pool.clone());

        let mut slot = oauth_slot_with_refresh(2, "rt_probe");
        slot.count_tokens_allowed = Some(true);
        slot.supports_claude_1m_sonnet = Some(true);
        slot.session_usage.total_input_tokens = 123;
        slot.lifetime_usage.total_output_tokens = 456;
        state.valid.push_back(slot.clone());
        state.probing.insert(2);

        let mut probe_runtime = slot.to_runtime_params();
        probe_runtime.count_tokens_allowed = None;
        probe_runtime.supports_claude_1m_sonnet = None;
        probe_runtime.buckets = Default::default();
        probe_runtime.session_has_reset = Some(true);
        probe_runtime.weekly_has_reset = Some(true);
        probe_runtime.session_utilization = Some(45.0);
        probe_runtime.weekly_utilization = Some(17.0);
        probe_runtime.resets_last_checked_at = Some(1_777_100_000);

        AccountPoolActor::collect_by_id(
            &mut state,
            2,
            probe_runtime,
            None,
            Some(CredentialFingerprint::from_oauth_refresh_token("rt_probe")),
            RuntimeMergeMode::OAuthSnapshot,
        );

        assert!(
            !state.probing.contains(&2),
            "probe runtime release must complete the probing overlay"
        );

        AccountPoolActor::do_flush(&mut state).await;
        let accounts = load_all_accounts(&pool).await.unwrap();
        let runtime = accounts
            .iter()
            .find(|account| account.id == 2)
            .and_then(|account| account.runtime.as_ref())
            .expect("runtime row must be flushed");

        assert_eq!(runtime.session_utilization, Some(45.0));
        assert_eq!(runtime.weekly_utilization, Some(17.0));
        assert_eq!(runtime.resets_last_checked_at, Some(1_777_100_000));
        assert_eq!(
            runtime.count_tokens_allowed,
            Some(true),
            "OAuth snapshot release must preserve local capability probes"
        );
        assert_eq!(runtime.buckets[0].total_input_tokens, 123);
        assert_eq!(runtime.buckets[4].total_output_tokens, 456);
    }

    /// C5 race scenario 3: admin reconnected an OAuth account, rotating
    /// BOTH access_token and refresh_token. The caller's release carries
    /// a fingerprint from the old refresh_token, so the guard fires and
    /// the runtime + Reason are both dropped.
    #[tokio::test]
    async fn collect_by_id_drops_stale_release_after_oauth_admin_reconnect() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);

        // Pool slot post admin reconnect: new refresh_token.
        let mut pool_slot = oauth_slot_with_refresh(3, "rt_new_after_admin_reconnect");
        pool_slot.count_tokens_allowed = Some(false);
        state.valid.push_back(pool_slot);

        // Caller captured fingerprint before reconnect.
        let request_time_slot = oauth_slot_with_refresh(3, "rt_old_pre_reconnect");
        let stale_fp = CredentialFingerprint::from_slot(&request_time_slot);

        // Runtime update from the stale request would flip count_tokens_allowed
        // to true AND demote with a Reason::TooManyRequest cooldown.
        let mut update = request_time_slot.to_runtime_params();
        update.count_tokens_allowed = Some(true);
        let cooldown_until = chrono::Utc::now().timestamp() + 7200;
        AccountPoolActor::collect_by_id(
            &mut state,
            3,
            update,
            Some(Reason::TooManyRequest(cooldown_until)),
            stale_fp,
            RuntimeMergeMode::Full,
        );

        let after = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(3))
            .expect("OAuth slot must stay in valid (cooldown was on stale credential)");
        assert_eq!(
            after.count_tokens_allowed,
            Some(false),
            "stale runtime must NOT overwrite the post-reconnect runtime"
        );
        assert!(
            !state.exhausted.contains_key(&3),
            "stale TMR cooldown must not push the new credential to exhausted"
        );
    }

    /// Backward compatibility: callers that pass `None` for fingerprint
    /// (probe paths still being wired through C6, plus historical test
    /// fixtures) keep the pre-C5 behavior — the guard becomes a
    /// pass-through and the update + Reason are applied as before.
    #[tokio::test]
    async fn collect_by_id_with_no_fingerprint_skips_guard_and_applies_update() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);
        state
            .valid
            .push_back(cookie_slot_with_blob(4, &cookie_blob_for(b'a')));

        let mut update = state.valid.front().unwrap().to_runtime_params();
        update.count_tokens_allowed = Some(true);
        AccountPoolActor::collect_by_id(&mut state, 4, update, None, None, RuntimeMergeMode::Full);

        let after = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(4))
            .unwrap();
        assert_eq!(after.count_tokens_allowed, Some(true));
    }

    /// Step 4 / C6: `do_reload` no longer mints a placeholder cookie
    /// just to land an oauth-only `disabled`/`auth_error` row in the
    /// invalid bucket. The bucket entry is built directly from
    /// `(row.id, AuthMethod::from_auth_source, Reason::from_db_string)`.
    /// This test runs both kinds through one reload so the auth_method
    /// stamping is per-row, not stuck on a constant.
    #[tokio::test]
    async fn reload_inserts_invalid_bucket_entries_without_credential_bytes() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        // Cookie account that just got auth_error'd by a probe.
        let cookie_blob = cookie_blob_for(b'e');
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source, cookie_blob,
                organization_uuid, drain_first, invalid_reason
            ) VALUES (?1, ?2, ?1, 5, 'auth_error', 'cookie', ?3, 'org', 0, 'null')",
        )
        .bind(70_i64)
        .bind("acc-70")
        .bind(&cookie_blob)
        .execute(&pool)
        .await
        .unwrap();

        // OAuth account that's been admin-disabled.
        insert_account_row(
            &pool,
            71,
            "disabled",
            "oauth",
            Some("at_x"),
            Some("rt_x"),
            Some("disabled"),
        )
        .await;

        let mut state = empty_state(pool);
        AccountPoolActor::do_reload(&mut state).await;

        let cookie_inv = state
            .invalid
            .get(&70)
            .expect("cookie auth_error row must land in invalid");
        assert_eq!(cookie_inv.account_id, 70);
        assert_eq!(cookie_inv.auth_method, AuthMethod::Cookie);
        assert_eq!(cookie_inv.reason, Reason::Null);

        let oauth_inv = state
            .invalid
            .get(&71)
            .expect("oauth disabled row must land in invalid");
        assert_eq!(oauth_inv.account_id, 71);
        assert_eq!(oauth_inv.auth_method, AuthMethod::OAuth);
        assert_eq!(oauth_inv.reason, Reason::Disabled);
    }

    /// Pool-side fingerprint lookup must NOT fall back to invalid-bucket
    /// cookie bytes after C6 — those bytes are gone. Returning `None` for
    /// invalid-only accounts is the correct behavior; the sticky-reason
    /// guard above `pool_credential_fingerprint` already covers every
    /// reason that can place an account in invalid (Free / Disabled /
    /// Banned / Null), so a None here cannot mask a stale-write race.
    #[tokio::test]
    async fn pool_credential_fingerprint_returns_none_for_invalid_only_accounts() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        let mut state = empty_state(pool);
        state.invalid.insert(
            99,
            crate::config::InvalidAccountSlot::new(99, AuthMethod::Cookie, Reason::Disabled),
        );

        let fp = AccountPoolActor::pool_credential_fingerprint(&state, 99);
        assert!(
            fp.is_none(),
            "invalid-only accounts must not synthesize a fingerprint from retired cookie bytes"
        );

        // Likewise for OAuth invalid (no token in invalid post-C6).
        state.invalid.insert(
            100,
            crate::config::InvalidAccountSlot::new(100, AuthMethod::OAuth, Reason::Banned),
        );
        let fp_oauth = AccountPoolActor::pool_credential_fingerprint(&state, 100);
        assert!(fp_oauth.is_none());
    }

    /// Step 4 / C8: loader no longer mints `oauth_placeholder_cookie(...)`
    /// for OAuth-only DB rows. The reloaded slot must have `cookie = None`
    /// and `auth_method = OAuth`, with the credential bytes living in
    /// `slot.token` (set by the loader from `row.oauth_token`).
    #[tokio::test]
    async fn reload_builds_oauth_slot_without_placeholder_cookie() {
        let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
        insert_account_row(
            &pool,
            80,
            "active",
            "oauth",
            Some("at_real"),
            Some("rt_real"),
            None,
        )
        .await;

        let mut state = empty_state(pool);
        AccountPoolActor::do_reload(&mut state).await;

        let slot = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(80))
            .expect("oauth account 80 must load");
        assert_eq!(slot.auth_method, AuthMethod::OAuth);
        assert!(
            slot.cookie.is_none(),
            "post-C8: OAuth slots have no placeholder cookie blob, got: {:?}",
            slot.cookie
        );
        assert_eq!(
            slot.token.as_ref().map(|t| t.access_token.as_str()),
            Some("at_real"),
            "OAuth credential bytes must live in slot.token"
        );
        assert_eq!(
            slot.token.as_ref().map(|t| t.refresh_token.as_str()),
            Some("rt_real"),
        );
    }

    /// `AccountSlot::oauth(id, token)` is the post-C8 canonical OAuth
    /// constructor. Pin its shape so future call sites don't drift back
    /// to placeholder-cookie idioms.
    #[test]
    fn account_slot_oauth_constructor_shape() {
        let token = TokenInfo::from_parts(
            "at_x".to_string(),
            "rt_x".to_string(),
            Duration::from_secs(3600),
            "org-x".to_string(),
        );
        let slot = AccountSlot::oauth(123, token);
        assert_eq!(slot.auth_method, AuthMethod::OAuth);
        assert!(slot.cookie.is_none());
        assert_eq!(slot.account_id, Some(123));
        assert_eq!(
            slot.token.as_ref().map(|t| t.access_token.as_str()),
            Some("at_x")
        );
        // credential_label uses the post-C7 OAuth tag, never reaches into
        // the (now-None) cookie field.
        assert_eq!(slot.credential_label(), "oauth#123");
    }
}
