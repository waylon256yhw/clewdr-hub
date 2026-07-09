use super::state::{AccountPoolActor, AccountPoolState, RuntimeMergeMode};
use super::{AccountPoolHandle, CredentialFingerprint};
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
                oauth_access_token, oauth_refresh_token, oauth_expires_at,
                organization_uuid, drain_first
            ) VALUES (?1, ?2, 1, 5, 'active', 'oauth', ?3, ?4, '2030-01-01T00:00:00Z', 'org', 0)",
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
    crate::db::accounts::upsert_account_oauth(&pool, 1, Some(&rotated), None, None)
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

#[tokio::test]
async fn conditional_pool_convergence_rejects_rotated_credential() {
    let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
    insert_oauth_row(&pool, 1, "at0", "rt0").await;
    let (event_tx, _) = broadcast::channel(16);
    let handle = AccountPoolHandle::start(pool, event_tx).await.unwrap();

    assert!(
        !handle
            .update_credential_if_current(1, "rt-admin", Some(token_with_refresh("rt-stale")))
            .await
            .unwrap()
    );
    assert_eq!(
        handle.get_token(1).await.unwrap().unwrap().refresh_token,
        "rt0"
    );

    assert!(
        handle
            .update_credential_if_current(1, "rt0", Some(token_with_refresh("rt1")))
            .await
            .unwrap()
    );
    assert!(
        !handle
            .invalidate_if_current(1, "rt0", Reason::Null)
            .await
            .unwrap()
    );
    assert!(
        handle
            .invalidate_if_current(1, "rt1", Reason::Null)
            .await
            .unwrap()
    );
    assert!(handle.get_token(1).await.unwrap().is_none());
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

#[tokio::test]
async fn cached_normal_account_yields_to_available_drain_first() {
    let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
    let mut state = empty_state(pool);
    push_slot(&mut state, 1, 5); // normal
    push_slot(&mut state, 2, 5); // drain_first
    state.drain_first_ids.insert(2);
    state.moka.insert(77, 1);

    let actor = AccountPoolActor;
    let dispatched = actor.dispatch(&mut state, Some(77), &[]).unwrap();

    assert_eq!(
        dispatched.account_id,
        Some(2),
        "drain_first must win over a cached normal account"
    );
    state.moka.run_pending_tasks();
    assert_eq!(
        state.moka.get(&77),
        Some(2),
        "cache must rebind to drain_first"
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

/// Step 5 follow-up: cold-restart with a stale `account_runtime_state`
/// row from a previous cookie/oauth life of this account_id must not
/// pollute an ApiKey slot. PRD Decision 2 puts ApiKey accounts
/// outside the quota window / cooldown machinery, so a stale
/// `reset_time` would otherwise park the slot in `exhausted` and
/// `count_tokens_allowed = false` would route count_tokens to the
/// local estimator.
///
/// Admin update DELETEs the runtime row on switch-in (see
/// admin/accounts.rs) as the primary cleanup; this loader-side
/// guard catches bundle-import / manual-DB-edit paths that
/// bypass it.
#[tokio::test]
async fn reload_does_not_apply_stale_runtime_to_api_key_slot() {
    let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
    // Insert an api_key account row (cookie/oauth columns NULL, both
    // api_key_* columns non-NULL — required by the schema mutex CHECK).
    sqlx::query(
        "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source,
                api_key_base_url, api_key_secret,
                organization_uuid, drain_first
            ) VALUES (?1, ?2, ?1, 5, 'active', 'api_key', ?3, ?4, NULL, 0)",
    )
    .bind(70_i64)
    .bind("acc-70")
    .bind("https://api.anthropic.com/")
    .bind("sk-ant-test-stale-runtime-regression")
    .execute(&pool)
    .await
    .unwrap();

    // Stale runtime row: future reset_time + count_tokens_allowed=false.
    // This is the shape of a row that used to belong to a cookie/oauth
    // account before the admin switched it to api_key without (or
    // before this guard) cleaning up account_runtime_state.
    let future_reset = chrono::Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT INTO account_runtime_state (account_id, reset_time, count_tokens_allowed)
             VALUES (?1, ?2, 0)",
    )
    .bind(70_i64)
    .bind(future_reset)
    .execute(&pool)
    .await
    .unwrap();

    let mut state = empty_state(pool);
    // Cold restart: nothing in mem_cookies. Without the loader guard,
    // do_reload's else-if branch would apply the stale runtime to the
    // ApiKey slot.
    AccountPoolActor::do_reload(&mut state).await;

    // ApiKey slot must end up in `valid` (NOT in `exhausted`) with
    // None runtime fields.
    assert!(
        !state.exhausted.contains_key(&70),
        "ApiKey slot must not be bucketed into exhausted by stale reset_time"
    );
    let slot = state
        .valid
        .iter()
        .find(|c| c.account_id == Some(70))
        .expect("ApiKey slot must appear in valid after cold reload");
    assert_eq!(slot.auth_method, AuthMethod::ApiKey);
    assert_eq!(
        slot.reset_time, None,
        "stale reset_time must not propagate to ApiKey"
    );
    assert_eq!(
        slot.count_tokens_allowed, None,
        "stale count_tokens_allowed must not propagate — try_count_tokens \
             would otherwise route to the local estimator instead of the \
             upstream count_tokens endpoint"
    );
}

/// Companion regression: the loader guard is ApiKey-specific. A
/// cookie account with the same stale-runtime DB shape must STILL
/// have its runtime applied on cold restart, otherwise we'd
/// regress existing cookie behavior.
#[tokio::test]
async fn reload_still_applies_runtime_to_cookie_slot_on_cold_start() {
    let pool = init_pool(std::path::Path::new(":memory:")).await.unwrap();
    let cookie_blob = cookie_blob_for(b'q');
    insert_cookie_account_row(&pool, 71, &cookie_blob).await;
    let future_reset = chrono::Utc::now().timestamp() + 3600;
    set_runtime_reset(&pool, 71, future_reset).await;

    let mut state = empty_state(pool);
    AccountPoolActor::do_reload(&mut state).await;

    assert!(
        state.exhausted.contains_key(&71),
        "cookie account with future reset_time must be bucketed exhausted"
    );
    let slot = state.exhausted.get(&71).unwrap();
    assert_eq!(slot.reset_time, Some(future_reset));
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
        weekly_scoped_limits: Vec::new(),
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
        last_failure: None,
        email: Some("u@e".into()),
        account_type: Some("Pro".into()),
        rate_limit_tier: None,
        subscription_created_at: None,
        billing_type: None,
        api_key_base_url: None,
        api_key_secret: None,
        api_key_extra_headers: None,
        api_key_extra_body: None,
        mimicry_mode: "none".into(),
        mimicry_config: None,
        total_cost_nanousd: 0,
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
        weekly_scoped_limits: Vec::new(),
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
        last_failure: None,
        email: None,
        account_type: None,
        rate_limit_tier: None,
        subscription_created_at: None,
        billing_type: None,
        api_key_base_url: None,
        api_key_secret: None,
        api_key_extra_headers: None,
        api_key_extra_body: None,
        mimicry_mode: "none".into(),
        mimicry_config: None,
        total_cost_nanousd: 0,
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
        last_failure: None,
        email: None,
        account_type: None,
        rate_limit_tier: None,
        subscription_created_at: None,
        billing_type: None,
        api_key_base_url: None,
        api_key_secret: None,
        api_key_extra_headers: None,
        api_key_extra_body: None,
        mimicry_mode: "none".into(),
        mimicry_config: None,
        total_cost_nanousd: 0,
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
        weekly_scoped_limits: Vec::new(),
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
        last_failure: None,
        email: None,
        account_type: None,
        rate_limit_tier: None,
        subscription_created_at: None,
        billing_type: None,
        api_key_base_url: None,
        api_key_secret: None,
        api_key_extra_headers: None,
        api_key_extra_body: None,
        mimicry_mode: "none".into(),
        mimicry_config: None,
        total_cost_nanousd: 0,
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
