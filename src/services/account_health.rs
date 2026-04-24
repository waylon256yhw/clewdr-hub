//! Unified account health view for `/health`, admin overview, admin accounts
//! list and pool reload logging.
//!
//! The pool and the DB historically disagree on the shape of "account
//! status": the pool sorts accounts into `valid / exhausted / invalid`
//! buckets that reflect dispatch eligibility, while the DB carries
//! `accounts.status` (`active | auth_error | disabled`) plus
//! `account_runtime_state.reset_time` for cooldowns. This module builds a
//! single snapshot every consumer can read from.
//!
//! See `docs/account-normalization-2026-04-21.md` §Step 2.5.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::config::Reason;
use crate::db::accounts::{AccountWithRuntime, active_reset_time};

/// Base health state for a single account. Mutually exclusive.
///
/// `Probing` is intentionally *not* a variant here — probes run against
/// accounts in every pool bucket (see `spawn_probe_all` /
/// `spawn_probe_accounts` in `src/services/account_pool.rs`), so modelling
/// it as a variant would drop the underlying base state. It lives on
/// [`AccountHealth`] as an overlay flag instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AccountHealthState {
    Active,
    CoolingDown {
        reset_time: i64,
    },
    Invalid {
        kind: InvalidKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<Reason>,
    },
    Unconfigured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidKind {
    AuthError,
    Disabled,
}

/// Per-account health: base state plus overlay probe info.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountHealth {
    #[serde(flatten)]
    pub state: AccountHealthState,
    pub probing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_probe_error: Option<String>,
}

/// Pool-side context for a single account, passed to [`compose_health`].
///
/// The pool's bucket membership is authoritative — it reflects actions that
/// may not have been persisted yet (e.g., `reset` just moved an account from
/// exhausted back to valid but `do_flush` has not yet cleared the DB
/// `reset_time`; `collect` just inserted into `state.invalid` but DB
/// `status` is still `active`). `compose_health` reads the bucket first and
/// only falls back to the DB row when the account is not in any pool
/// bucket (e.g., newly inserted between reloads).
#[derive(Debug, Default, Clone)]
pub struct PoolAccountView<'a> {
    /// Which pool bucket currently holds the account, or `None` if the
    /// account is not in the pool at all.
    pub bucket: Option<PoolBucket<'a>>,
    /// `(current_inflight, max_slots)` from `state.inflight`.
    pub inflight: Option<(u32, u32)>,
    /// Whether the account is currently being probed. Orthogonal overlay —
    /// does not change the base state.
    pub probing: bool,
    /// Transient probe error, cleared on probe success.
    pub last_probe_error: Option<&'a str>,
}

/// Which of the pool's three scheduling buckets an account occupies.
#[derive(Debug, Clone, Copy)]
pub enum PoolBucket<'a> {
    /// `state.valid` — account is in the dispatch rotation.
    Valid,
    /// `state.exhausted` — in cooldown. `reset_time` is the pool slot's
    /// reset_time, which may be fresher than `account_runtime_state`.
    Exhausted { reset_time: Option<i64> },
    /// `state.invalid` — dispatch-ineligible. `reason` is the in-memory
    /// `InvalidAccountSlot.reason`, which is authoritative even if DB
    /// `invalid_reason` has not been flushed yet.
    Invalid { reason: &'a Reason },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AccountHealthSummary {
    pub total: i64,
    pub pool: PoolCounts,
    pub detail: HealthDetail,
    pub invalid_breakdown: InvalidBreakdown,
    pub probe: ProbeSummary,
    pub auth_sources: AuthSourceCounts,
    pub generated_at: i64,
}

/// Legacy three-bucket counts, derived from the per-account health
/// states so they cannot disagree with [`HealthDetail`] or the
/// dispatcher's real view:
///
/// - `valid` — accounts currently [`AccountHealthState::Active`],
///   including those in `state.exhausted` whose `reset_time` has
///   already passed (the pool's reset tick has not fired yet but
///   dispatch will reclaim them on its next attempt).
/// - `exhausted` — accounts currently [`AccountHealthState::CoolingDown`].
/// - `invalid` — accounts currently [`AccountHealthState::Invalid`].
///
/// Accounts with [`AccountHealthState::Unconfigured`] do not count toward
/// any of these — matching the pre-Step-2.5 meaning of
/// `state.{valid,exhausted,invalid}.len()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PoolCounts {
    pub valid: usize,
    pub exhausted: usize,
    pub invalid: usize,
}

/// Orthogonal diagnostic slices.
///
/// **Not** a partition of `total`: `probing` overlays cooling / invalid /
/// unconfigured accounts, so the fields can sum to more than `total`.
/// Consumers must not assert `sum == total`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct HealthDetail {
    pub dispatchable_now: usize,
    pub saturated: usize,
    pub cooling_down: usize,
    pub probing: usize,
    pub invalid_auth: usize,
    pub invalid_disabled: usize,
    pub unconfigured: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct InvalidBreakdown {
    pub free: i64,
    pub disabled: i64,
    pub banned: i64,
    pub null: i64,
    pub restricted: i64,
    pub too_many_request: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProbeSummary {
    pub probing_count: usize,
    pub probing_ids: Vec<i64>,
    pub last_errors: HashMap<i64, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AuthSourceCounts {
    pub oauth: i64,
    pub cookie: i64,
}

/// Wire type returned by [`compose_health_snapshot`].
///
/// Carries the aggregated summary plus the per-account view. Callers that
/// only need the summary (`/health`, admin overview, reload log) can ignore
/// `per_account`; the admin accounts list consumes both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AccountHealthSnapshot {
    pub summary: AccountHealthSummary,
    pub per_account: HashMap<i64, AccountHealth>,
}

/// Cheap in-memory snapshot of the pool fields needed to compose an
/// [`AccountHealthSnapshot`]. Produced in one actor turn by
/// `AccountPoolState::snapshot_view` without touching the DB, so the
/// health read path does not serialise with dispatch / return traffic
/// on the actor. The DB rows are loaded off-actor by the caller.
#[derive(Debug, Default, Clone)]
pub struct PoolSnapshotView {
    pub valid_ids: HashSet<i64>,
    /// `account_id → AccountSlot.reset_time` for rows currently in the
    /// exhausted bucket.
    pub exhausted: HashMap<i64, Option<i64>>,
    /// `account_id → InvalidAccountSlot.reason` for rows currently in
    /// the invalid bucket.
    pub invalid: HashMap<i64, Reason>,
    pub inflight: HashMap<i64, (u32, u32)>,
    pub probing: HashSet<i64>,
    pub probe_errors: HashMap<i64, String>,
}

fn has_credential(account: &AccountWithRuntime) -> bool {
    account.cookie_blob.as_ref().is_some_and(|v| !v.is_empty())
        || (account.auth_source == "oauth" && account.oauth_token.is_some())
}

fn parse_db_invalid_reason(account: &AccountWithRuntime) -> Option<Reason> {
    account
        .invalid_reason
        .as_deref()
        .and_then(Reason::from_db_string_checked)
}

/// Infer [`InvalidKind`] when the pool says an account is invalid but the
/// DB `status` has not caught up yet. `state.invalid` never holds
/// `TooManyRequest`/`Restricted` (those go to `exhausted`), so a bare
/// `Reason::Disabled` implies a disabled account and anything else
/// (Free/Banned/Null) implies auth_error.
fn infer_invalid_kind(account: &AccountWithRuntime, pool_reason: &Reason) -> InvalidKind {
    match account.status.as_str() {
        "disabled" => InvalidKind::Disabled,
        "auth_error" => InvalidKind::AuthError,
        _ => match pool_reason {
            Reason::Disabled => InvalidKind::Disabled,
            _ => InvalidKind::AuthError,
        },
    }
}

fn compose_from_db(account: &AccountWithRuntime) -> AccountHealthState {
    match account.status.as_str() {
        "auth_error" => AccountHealthState::Invalid {
            kind: InvalidKind::AuthError,
            reason: parse_db_invalid_reason(account),
        },
        "disabled" => AccountHealthState::Invalid {
            kind: InvalidKind::Disabled,
            reason: parse_db_invalid_reason(account),
        },
        _ if !has_credential(account) => AccountHealthState::Unconfigured,
        _ => match active_reset_time(account) {
            Some(reset) => AccountHealthState::CoolingDown { reset_time: reset },
            None => AccountHealthState::Active,
        },
    }
}

/// Derive the base health state for one account.
///
/// Precedence: the pool's bucket membership wins. DB is consulted only
/// when the account is not in any bucket (e.g., newly inserted row not yet
/// reloaded) or to refine `InvalidKind` when the pool's view alone is
/// ambiguous.
///
/// `now` is the UNIX timestamp used to filter expired cooldowns. An
/// account in `exhausted` whose `reset_time <= now` is reported as
/// `Active` — the pool's periodic `reset` tick will physically move it
/// back to `valid` on its next turn, but the dispatcher and every read
/// surface should already treat it as usable.
pub fn compose_health(
    account: &AccountWithRuntime,
    view: PoolAccountView<'_>,
    now: i64,
) -> AccountHealth {
    let state = match view.bucket {
        Some(PoolBucket::Valid) => AccountHealthState::Active,
        Some(PoolBucket::Exhausted { reset_time }) => {
            // Drop reset_time entries that have already expired — the
            // pool's CheckReset tick has not run yet (every 300s), but
            // the account is effectively ready.
            let effective = reset_time
                .filter(|t| *t > now)
                .or_else(|| active_reset_time(account));
            match effective {
                Some(t) => AccountHealthState::CoolingDown { reset_time: t },
                None => AccountHealthState::Active,
            }
        }
        Some(PoolBucket::Invalid { reason }) => AccountHealthState::Invalid {
            kind: infer_invalid_kind(account, reason),
            reason: Some(reason.clone()),
        },
        None => compose_from_db(account),
    };

    AccountHealth {
        state,
        probing: view.probing,
        last_probe_error: view.last_probe_error.map(str::to_owned),
    }
}

fn inflight_saturated(inflight: Option<(u32, u32)>) -> bool {
    match inflight {
        Some((cur, max)) => max > 0 && cur >= max,
        None => false,
    }
}

/// Aggregate per-account health into the full summary.
///
/// `pool_counts` and `probe` come straight from `AccountPoolState` — the
/// pool is authoritative for those. `inflight` is needed to classify
/// Active accounts into `dispatchable_now` vs `saturated`.
pub fn summarize(
    accounts: &[AccountWithRuntime],
    per_account: &HashMap<i64, AccountHealth>,
    inflight: &HashMap<i64, (u32, u32)>,
    pool_counts: PoolCounts,
    probe: ProbeSummary,
    generated_at: i64,
) -> AccountHealthSummary {
    let mut detail = HealthDetail::default();
    let mut invalid_breakdown = InvalidBreakdown::default();
    let mut auth_sources = AuthSourceCounts::default();

    for account in accounts {
        match account.auth_source.as_str() {
            "oauth" => auth_sources.oauth += 1,
            "cookie" => auth_sources.cookie += 1,
            _ => {}
        }

        let Some(health) = per_account.get(&account.id) else {
            continue;
        };

        if health.probing {
            detail.probing += 1;
        }

        match &health.state {
            AccountHealthState::Active => {
                // Probing is an orthogonal overlay — the dispatcher's
                // `is_usable` predicate does not exclude probing accounts
                // (see `src/services/account_pool.rs` `is_usable`), so
                // `dispatchable_now` must not either.
                if inflight_saturated(inflight.get(&account.id).copied()) {
                    detail.saturated += 1;
                } else {
                    detail.dispatchable_now += 1;
                }
            }
            AccountHealthState::CoolingDown { .. } => {
                detail.cooling_down += 1;
            }
            AccountHealthState::Invalid { kind, reason } => {
                match kind {
                    InvalidKind::AuthError => detail.invalid_auth += 1,
                    InvalidKind::Disabled => detail.invalid_disabled += 1,
                }
                if let Some(r) = reason {
                    match r {
                        Reason::Free => invalid_breakdown.free += 1,
                        Reason::Disabled => invalid_breakdown.disabled += 1,
                        Reason::Banned => invalid_breakdown.banned += 1,
                        Reason::Null => invalid_breakdown.null += 1,
                        Reason::Restricted(_) => invalid_breakdown.restricted += 1,
                        Reason::TooManyRequest(_) => invalid_breakdown.too_many_request += 1,
                    }
                }
            }
            AccountHealthState::Unconfigured => {
                detail.unconfigured += 1;
            }
        }
    }

    AccountHealthSummary {
        total: accounts.len() as i64,
        pool: pool_counts,
        detail,
        invalid_breakdown,
        probe,
        auth_sources,
        generated_at,
    }
}

/// End-to-end snapshot builder: join the loaded DB rows with the actor's
/// cheap in-memory snapshot to produce every read surface's view.
///
/// Pure — no IO, no locks — so it runs wherever the caller wants
/// (typically off-actor, right after a parallel `load_all_accounts` +
/// `SnapshotPoolState` RPC). `now` is the UNIX timestamp used both for
/// `generated_at` and for filtering expired cooldowns.
pub fn compose_health_snapshot(
    view: &PoolSnapshotView,
    accounts: &[AccountWithRuntime],
    now: i64,
) -> AccountHealthSnapshot {
    let mut per_account: HashMap<i64, AccountHealth> = HashMap::with_capacity(accounts.len());
    for account in accounts {
        let id = account.id;
        let bucket = if view.valid_ids.contains(&id) {
            Some(PoolBucket::Valid)
        } else if let Some(reset) = view.exhausted.get(&id) {
            Some(PoolBucket::Exhausted { reset_time: *reset })
        } else {
            view.invalid
                .get(&id)
                .map(|reason| PoolBucket::Invalid { reason })
        };
        let account_view = PoolAccountView {
            bucket,
            inflight: view.inflight.get(&id).copied(),
            probing: view.probing.contains(&id),
            last_probe_error: view.probe_errors.get(&id).map(String::as_str),
        };
        per_account.insert(id, compose_health(account, account_view, now));
    }

    // Derive pool counts from the per-account health so /health.ready,
    // admin overview's pool.{valid,exhausted,invalid} and detail.*
    // stay consistent: an expired cooldown moves out of `exhausted` into
    // `valid` here the same moment compose_health flipped its state to
    // Active, instead of lingering in `exhausted` until the next reset
    // tick (up to 300s). Accounts that are in DB but not in any pool
    // bucket (Unconfigured) do not count toward the legacy three-bucket
    // view — same as pre-Step-2.5.
    let mut pool_counts = PoolCounts::default();
    for health in per_account.values() {
        match &health.state {
            AccountHealthState::Active => pool_counts.valid += 1,
            AccountHealthState::CoolingDown { .. } => pool_counts.exhausted += 1,
            AccountHealthState::Invalid { .. } => pool_counts.invalid += 1,
            AccountHealthState::Unconfigured => {}
        }
    }

    let mut probing_ids: Vec<i64> = view.probing.iter().copied().collect();
    probing_ids.sort_unstable();
    let probe = ProbeSummary {
        probing_count: view.probing.len(),
        probing_ids,
        last_errors: view.probe_errors.clone(),
    };

    let summary = summarize(
        accounts,
        &per_account,
        &view.inflight,
        pool_counts,
        probe,
        now,
    );

    AccountHealthSnapshot {
        summary,
        per_account,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;

    use super::*;
    use crate::config::{Organization, TokenInfo, UsageBreakdown};

    fn now() -> i64 {
        Utc::now().timestamp()
    }
    use crate::db::accounts::RuntimeStateRow;

    fn runtime(reset_time: Option<i64>) -> RuntimeStateRow {
        RuntimeStateRow {
            reset_time,
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
            buckets: std::array::from_fn(|_| UsageBreakdown::default()),
        }
    }

    fn account(
        id: i64,
        auth_source: &str,
        status: &str,
        cookie_blob: Option<&str>,
        has_oauth: bool,
        reset_time: Option<i64>,
    ) -> AccountWithRuntime {
        AccountWithRuntime {
            id,
            name: format!("acct-{id}"),
            rr_order: id,
            max_slots: 5,
            proxy_id: None,
            proxy_name: None,
            proxy_url: None,
            drain_first: false,
            status: status.to_string(),
            auth_source: auth_source.to_string(),
            cookie_blob: cookie_blob.map(str::to_string),
            oauth_token: has_oauth.then(|| TokenInfo {
                access_token: "access".to_string(),
                expires_in: Duration::from_secs(3600),
                organization: Organization {
                    uuid: "org".to_string(),
                },
                refresh_token: "refresh".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
            }),
            oauth_expires_at: None,
            last_refresh_at: None,
            last_error: None,
            organization_uuid: Some("org".to_string()),
            invalid_reason: None,
            email: None,
            account_type: None,
            created_at: None,
            updated_at: None,
            runtime: Some(runtime(reset_time)),
        }
    }

    fn with_invalid_reason(mut a: AccountWithRuntime, reason: &Reason) -> AccountWithRuntime {
        a.invalid_reason = Some(reason.to_db_string());
        a
    }

    #[test]
    fn active_account_with_credential_is_active() {
        let a = account(1, "cookie", "active", Some("c=yes"), false, None);
        let h = compose_health(&a, PoolAccountView::default(), now());
        assert_eq!(h.state, AccountHealthState::Active);
        assert!(!h.probing);
        assert!(h.last_probe_error.is_none());
    }

    #[test]
    fn active_without_credential_is_unconfigured() {
        let a = account(1, "cookie", "active", None, false, None);
        let h = compose_health(&a, PoolAccountView::default(), now());
        assert_eq!(h.state, AccountHealthState::Unconfigured);
    }

    #[test]
    fn oauth_only_account_is_active_when_token_present() {
        let a = account(1, "oauth", "active", None, true, None);
        let h = compose_health(&a, PoolAccountView::default(), now());
        assert_eq!(h.state, AccountHealthState::Active);
    }

    #[test]
    fn past_reset_time_does_not_cool() {
        let past = Utc::now().timestamp() - 60;
        let a = account(1, "cookie", "active", Some("c=yes"), false, Some(past));
        let h = compose_health(&a, PoolAccountView::default(), now());
        assert_eq!(h.state, AccountHealthState::Active);
    }

    #[test]
    fn future_db_reset_time_cools_down() {
        let future = Utc::now().timestamp() + 300;
        let a = account(1, "cookie", "active", Some("c=yes"), false, Some(future));
        let h = compose_health(&a, PoolAccountView::default(), now());
        assert_eq!(
            h.state,
            AccountHealthState::CoolingDown { reset_time: future }
        );
    }

    #[test]
    fn pool_exhausted_bucket_overrides_db_reset_time() {
        let db_reset = Utc::now().timestamp() + 300;
        let pool_reset = Utc::now().timestamp() + 600;
        let a = account(1, "cookie", "active", Some("c=yes"), false, Some(db_reset));
        let view = PoolAccountView {
            bucket: Some(PoolBucket::Exhausted {
                reset_time: Some(pool_reset),
            }),
            ..Default::default()
        };
        let h = compose_health(&a, view, now());
        assert_eq!(
            h.state,
            AccountHealthState::CoolingDown {
                reset_time: pool_reset
            }
        );
    }

    #[test]
    fn pool_exhausted_bucket_with_expired_reset_reports_active() {
        // Regression: the pool's reset() tick runs every 300s. Between the
        // moment a cooldown expires and the next tick, the account is
        // still physically in state.exhausted with reset_time in the past.
        // Without this guard /health, admin overview and admin accounts
        // would keep reporting it as cooling_down for up to five minutes
        // even though the dispatcher is free to use it.
        let past = Utc::now().timestamp() - 60;
        let a = account(1, "oauth", "active", None, true, None);
        let view = PoolAccountView {
            bucket: Some(PoolBucket::Exhausted {
                reset_time: Some(past),
            }),
            ..Default::default()
        };
        let h = compose_health(&a, view, Utc::now().timestamp());
        assert_eq!(h.state, AccountHealthState::Active);
    }

    #[test]
    fn expired_exhausted_bucket_does_not_count_as_cooling_in_snapshot() {
        // Roll the same regression through compose_health_snapshot so the
        // overview / admin detail counts stay honest too.
        let past = Utc::now().timestamp() - 60;
        let a = account(1, "oauth", "active", None, true, None);
        let mut exhausted = HashMap::new();
        exhausted.insert(1, Some(past));
        let view = PoolSnapshotView {
            exhausted,
            ..Default::default()
        };
        let snap = compose_health_snapshot(&view, &[a], Utc::now().timestamp());
        assert_eq!(snap.summary.detail.cooling_down, 0);
        assert_eq!(snap.summary.detail.dispatchable_now, 1);
        assert_eq!(
            snap.per_account.get(&1).map(|h| h.state.clone()),
            Some(AccountHealthState::Active)
        );
        // Legacy pool counts also reflect the reclassification, so
        // /health.ready (valid > 0) and admin overview.pool.valid do not
        // disagree with detail.dispatchable_now.
        assert_eq!(snap.summary.pool.valid, 1);
        assert_eq!(snap.summary.pool.exhausted, 0);
    }

    #[test]
    fn expired_cooldown_makes_health_ready_without_waiting_for_reset_tick() {
        // A single-account deployment whose only account has just cleared
        // its cooldown must not continue reporting /health.ready=false
        // until the 300s reset tick fires.
        let past = Utc::now().timestamp() - 1;
        let a = account(1, "oauth", "active", None, true, None);
        let mut exhausted = HashMap::new();
        exhausted.insert(1, Some(past));
        let view = PoolSnapshotView {
            exhausted,
            ..Default::default()
        };
        let snap = compose_health_snapshot(&view, &[a], Utc::now().timestamp());
        assert!(
            snap.summary.pool.valid > 0,
            "pool.valid drives /health.ready"
        );
    }

    #[test]
    fn pool_valid_bucket_overrides_stale_db_reset_time() {
        // Regression: after `reset()` moves an account from `exhausted` back
        // to `valid` in memory, the DB `runtime.reset_time` may still be in
        // the future until the next `do_flush`. The snapshot must trust the
        // pool's bucket (Valid → Active), not the DB view.
        let stale_future = Utc::now().timestamp() + 900;
        let a = account(
            1,
            "cookie",
            "active",
            Some("c=yes"),
            false,
            Some(stale_future),
        );
        let view = PoolAccountView {
            bucket: Some(PoolBucket::Valid),
            ..Default::default()
        };
        let h = compose_health(&a, view, now());
        assert_eq!(h.state, AccountHealthState::Active);
    }

    #[test]
    fn pool_invalid_bucket_overrides_active_db_status() {
        // Regression: between `collect(reason=Banned)` inserting into
        // `state.invalid` and `do_flush` persisting `status='auth_error'`,
        // the DB row still reads `status='active'`. The snapshot must
        // trust the pool and report Invalid, not Active.
        let a = account(1, "cookie", "active", Some("c=yes"), false, None);
        let view = PoolAccountView {
            bucket: Some(PoolBucket::Invalid {
                reason: &Reason::Banned,
            }),
            ..Default::default()
        };
        let h = compose_health(&a, view, now());
        assert_eq!(
            h.state,
            AccountHealthState::Invalid {
                kind: InvalidKind::AuthError,
                reason: Some(Reason::Banned),
            }
        );
    }

    #[test]
    fn pool_invalid_bucket_with_disabled_reason_picks_disabled_kind() {
        // When the DB hasn't caught up, infer InvalidKind from the pool
        // reason: Reason::Disabled → InvalidKind::Disabled.
        let a = account(1, "cookie", "active", Some("c=yes"), false, None);
        let view = PoolAccountView {
            bucket: Some(PoolBucket::Invalid {
                reason: &Reason::Disabled,
            }),
            ..Default::default()
        };
        let h = compose_health(&a, view, now());
        assert_eq!(
            h.state,
            AccountHealthState::Invalid {
                kind: InvalidKind::Disabled,
                reason: Some(Reason::Disabled),
            }
        );
    }

    #[test]
    fn auth_error_maps_to_invalid_with_reason() {
        let a = with_invalid_reason(
            account(1, "cookie", "auth_error", Some("c=yes"), false, None),
            &Reason::Free,
        );
        let h = compose_health(&a, PoolAccountView::default(), now());
        assert_eq!(
            h.state,
            AccountHealthState::Invalid {
                kind: InvalidKind::AuthError,
                reason: Some(Reason::Free),
            }
        );
    }

    #[test]
    fn disabled_maps_to_invalid_disabled_reason() {
        let a = with_invalid_reason(
            account(1, "oauth", "disabled", None, true, None),
            &Reason::Banned,
        );
        let h = compose_health(&a, PoolAccountView::default(), now());
        assert_eq!(
            h.state,
            AccountHealthState::Invalid {
                kind: InvalidKind::Disabled,
                reason: Some(Reason::Banned),
            }
        );
    }

    #[test]
    fn invalid_without_reason_keeps_none() {
        let a = account(1, "oauth", "disabled", None, true, None);
        let h = compose_health(&a, PoolAccountView::default(), now());
        assert_eq!(
            h.state,
            AccountHealthState::Invalid {
                kind: InvalidKind::Disabled,
                reason: None,
            }
        );
    }

    #[test]
    fn probing_overlays_cooling_down_without_changing_base() {
        let future = Utc::now().timestamp() + 300;
        let a = account(1, "cookie", "active", Some("c=yes"), false, Some(future));
        let view = PoolAccountView {
            probing: true,
            last_probe_error: Some("boom"),
            ..Default::default()
        };
        let h = compose_health(&a, view, now());
        assert_eq!(
            h.state,
            AccountHealthState::CoolingDown { reset_time: future }
        );
        assert!(h.probing);
        assert_eq!(h.last_probe_error.as_deref(), Some("boom"));
    }

    #[test]
    fn probing_overlays_invalid_without_changing_base() {
        let a = account(1, "oauth", "disabled", None, true, None);
        let view = PoolAccountView {
            probing: true,
            ..Default::default()
        };
        let h = compose_health(&a, view, now());
        assert!(h.probing);
        assert!(matches!(
            h.state,
            AccountHealthState::Invalid {
                kind: InvalidKind::Disabled,
                ..
            }
        ));
    }

    #[test]
    fn probing_overlays_unconfigured_without_changing_base() {
        let a = account(1, "cookie", "active", None, false, None);
        let view = PoolAccountView {
            probing: true,
            last_probe_error: Some("still no cookie"),
            ..Default::default()
        };
        let h = compose_health(&a, view, now());
        assert_eq!(h.state, AccountHealthState::Unconfigured);
        assert!(h.probing);
        assert_eq!(h.last_probe_error.as_deref(), Some("still no cookie"));
    }

    fn per_account_map(pairs: Vec<(i64, AccountHealth)>) -> HashMap<i64, AccountHealth> {
        pairs.into_iter().collect()
    }

    #[test]
    fn summarize_classifies_active_dispatchable_vs_saturated() {
        let dispatchable = account(1, "cookie", "active", Some("c=yes"), false, None);
        let saturated = account(2, "oauth", "active", None, true, None);
        let accounts = vec![dispatchable.clone(), saturated.clone()];
        let per_account = per_account_map(vec![
            (
                1,
                compose_health(
                    &dispatchable,
                    PoolAccountView {
                        bucket: Some(PoolBucket::Valid),
                        ..Default::default()
                    },
                    now(),
                ),
            ),
            (
                2,
                compose_health(
                    &saturated,
                    PoolAccountView {
                        bucket: Some(PoolBucket::Valid),
                        ..Default::default()
                    },
                    now(),
                ),
            ),
        ]);
        let mut inflight = HashMap::new();
        inflight.insert(2, (5u32, 5u32));

        let summary = summarize(
            &accounts,
            &per_account,
            &inflight,
            PoolCounts {
                valid: 2,
                ..Default::default()
            },
            ProbeSummary::default(),
            0,
        );

        assert_eq!(summary.total, 2);
        assert_eq!(summary.detail.dispatchable_now, 1);
        assert_eq!(summary.detail.saturated, 1);
        assert_eq!(summary.detail.cooling_down, 0);
        assert_eq!(summary.detail.probing, 0);
    }

    #[test]
    fn active_probing_unsaturated_still_counts_as_dispatchable() {
        // Regression: the dispatcher's `is_usable` predicate does not
        // exclude probing accounts; `dispatchable_now` must match that
        // reality. Otherwise an active+probing+unsaturated account
        // silently disappears from the count even though requests will
        // still be routed to it.
        let a = account(1, "cookie", "active", Some("c=yes"), false, None);
        let view = PoolAccountView {
            bucket: Some(PoolBucket::Valid),
            probing: true,
            last_probe_error: Some("retry"),
            ..Default::default()
        };
        let health = compose_health(&a, view, now());
        assert_eq!(health.state, AccountHealthState::Active);
        assert!(health.probing);

        let per_account = per_account_map(vec![(1, health)]);
        let probe = ProbeSummary {
            probing_count: 1,
            probing_ids: vec![1],
            last_errors: {
                let mut m = HashMap::new();
                m.insert(1, "retry".to_string());
                m
            },
        };
        let summary = summarize(
            &[a],
            &per_account,
            &HashMap::new(),
            PoolCounts {
                valid: 1,
                ..Default::default()
            },
            probe,
            0,
        );
        assert_eq!(summary.detail.dispatchable_now, 1);
        assert_eq!(summary.detail.saturated, 0);
        assert_eq!(summary.detail.probing, 1);
    }

    #[test]
    fn summarize_probing_is_orthogonal_and_does_not_double_count_base() {
        // Disabled + probing overlay: must count as invalid_disabled AND probing.
        let disabled = with_invalid_reason(
            account(1, "cookie", "disabled", Some("c=yes"), false, None),
            &Reason::Banned,
        );
        let future = Utc::now().timestamp() + 300;
        let cooling = account(2, "cookie", "active", Some("c=yes"), false, Some(future));

        let accounts = vec![disabled.clone(), cooling.clone()];
        let per_account = per_account_map(vec![
            (
                1,
                compose_health(
                    &disabled,
                    PoolAccountView {
                        probing: true,
                        last_probe_error: Some("retry"),
                        ..Default::default()
                    },
                    now(),
                ),
            ),
            (
                2,
                compose_health(
                    &cooling,
                    PoolAccountView {
                        probing: true,
                        ..Default::default()
                    },
                    now(),
                ),
            ),
        ]);

        let probe = ProbeSummary {
            probing_count: 2,
            probing_ids: vec![1, 2],
            last_errors: {
                let mut m = HashMap::new();
                m.insert(1, "retry".to_string());
                m
            },
        };
        let summary = summarize(
            &accounts,
            &per_account,
            &HashMap::new(),
            PoolCounts::default(),
            probe,
            0,
        );

        assert_eq!(summary.detail.probing, 2);
        assert_eq!(summary.detail.invalid_disabled, 1);
        assert_eq!(summary.detail.cooling_down, 1);
        assert_eq!(summary.detail.invalid_auth, 0);
        assert_eq!(summary.invalid_breakdown.banned, 1);
        assert_eq!(summary.probe.probing_ids, vec![1, 2]);
    }

    #[test]
    fn summarize_counts_auth_sources_and_invalid_breakdown() {
        let auth_err = with_invalid_reason(
            account(1, "cookie", "auth_error", Some("c=yes"), false, None),
            &Reason::Free,
        );
        let disabled = with_invalid_reason(
            account(2, "oauth", "disabled", None, true, None),
            &Reason::Banned,
        );
        let active = account(3, "oauth", "active", None, true, None);
        let accounts = vec![auth_err.clone(), disabled.clone(), active.clone()];
        let per_account = per_account_map(vec![
            (
                1,
                compose_health(&auth_err, PoolAccountView::default(), now()),
            ),
            (
                2,
                compose_health(&disabled, PoolAccountView::default(), now()),
            ),
            (
                3,
                compose_health(&active, PoolAccountView::default(), now()),
            ),
        ]);

        let summary = summarize(
            &accounts,
            &per_account,
            &HashMap::new(),
            PoolCounts::default(),
            ProbeSummary::default(),
            0,
        );

        assert_eq!(summary.auth_sources.oauth, 2);
        assert_eq!(summary.auth_sources.cookie, 1);
        assert_eq!(summary.invalid_breakdown.free, 1);
        assert_eq!(summary.invalid_breakdown.banned, 1);
        assert_eq!(summary.detail.invalid_auth, 1);
        assert_eq!(summary.detail.invalid_disabled, 1);
        assert_eq!(summary.detail.dispatchable_now, 1);
    }

    #[test]
    fn serialize_shape_flattens_state_into_account_health() {
        let h = AccountHealth {
            state: AccountHealthState::CoolingDown { reset_time: 123 },
            probing: true,
            last_probe_error: Some("transient".to_string()),
        };
        let json = serde_json::to_value(&h).unwrap();
        assert_eq!(json["state"], "cooling_down");
        assert_eq!(json["reset_time"], 123);
        assert_eq!(json["probing"], true);
        assert_eq!(json["last_probe_error"], "transient");
    }

    #[test]
    fn serialize_invalid_omits_reason_when_none() {
        let h = AccountHealth {
            state: AccountHealthState::Invalid {
                kind: InvalidKind::AuthError,
                reason: None,
            },
            probing: false,
            last_probe_error: None,
        };
        let json = serde_json::to_value(&h).unwrap();
        assert_eq!(json["state"], "invalid");
        assert_eq!(json["kind"], "auth_error");
        assert!(json.get("reason").is_none());
        assert_eq!(json["probing"], false);
    }
}
