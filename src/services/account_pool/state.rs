use std::collections::{HashMap, HashSet, VecDeque};

use chrono::Utc;
use moka::sync::Cache;
use ractor::RpcReplyPort;
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use crate::{
    config::{
        AccountSlot, InvalidAccountSlot, Reason, RuntimeStateParams, TokenInfo, UsageBreakdown,
    },
    error::ClewdrError,
    services::account_health::PoolSnapshotView,
    state::AdminEvent,
};

use super::{AccountPoolHandle, CredentialFingerprint};

const SESSION_WINDOW_SECS: i64 = 5 * 60 * 60; // 5h
const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 60 * 60; // 7d

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
pub(super) enum RuntimeMergeMode {
    Full,
    OAuthSnapshot,
}

#[derive(Debug)]
pub(super) enum AccountPoolMessage {
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
    UpdateCredentialIfCurrent(i64, String, Option<TokenInfo>, RpcReplyPort<bool>),
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
    InvalidateIfCurrent(i64, String, Reason, RpcReplyPort<bool>),
    /// Return a cheap in-memory pool snapshot for the health read path,
    /// along with the actor's DB handle. The caller runs
    /// `load_all_accounts` and `account_health::compose_health_snapshot`
    /// off-actor, so the `/health` / overview / accounts list endpoints
    /// do not serialise with dispatch / return traffic on this actor.
    /// See `docs/account-normalization-2026-04-21.md` §Step 2.5.
    SnapshotPoolState(RpcReplyPort<(PoolSnapshotView, SqlitePool)>),
}

#[derive(Debug)]
pub(super) struct AccountPoolState {
    pub(super) valid: VecDeque<AccountSlot>,
    pub(super) exhausted: HashMap<i64, AccountSlot>,
    pub(super) invalid: HashMap<i64, InvalidAccountSlot>,
    pub(super) moka: Cache<u64, i64>,
    pub(super) db: SqlitePool,
    pub(super) event_tx: broadcast::Sender<AdminEvent>,
    pub(super) dirty: HashSet<i64>,
    pub(super) handle: Option<AccountPoolHandle>,
    /// Per-account inflight tracking: account_id → (current_inflight, max_slots)
    pub(super) inflight: HashMap<i64, (u32, u32)>,
    pub(super) probing: HashSet<i64>,
    pub(super) reactivated: HashSet<i64>,
    /// Last probe error per account (transient errors only, cleared on success)
    pub(super) probe_errors: HashMap<i64, String>,
    /// Account IDs marked with `drain_first = true`. These are preferred
    /// during dispatch until all of them have no available inflight slot.
    pub(super) drain_first_ids: HashSet<i64>,
}

pub(super) struct AccountPoolActor;

impl AccountPoolActor {
    pub(super) fn emit_accounts_refresh(state: &AccountPoolState) {
        let _ = state.event_tx.send(AdminEvent::accounts_refresh());
    }

    pub(super) fn mark_dirty(state: &mut AccountPoolState, account_id: Option<i64>) {
        if let Some(id) = account_id {
            state.dirty.insert(id);
        }
    }

    pub(super) fn mark_all_dirty(state: &mut AccountPoolState) {
        for cs in state.valid.iter().chain(state.exhausted.values()) {
            if let Some(id) = cs.account_id {
                state.dirty.insert(id);
            }
        }
        for uc in state.invalid.values() {
            state.dirty.insert(uc.account_id);
        }
    }

    pub(super) fn reset(state: &mut AccountPoolState) {
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

    pub(super) fn refresh_usage_windows(state: &mut AccountPoolState) -> bool {
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
}
