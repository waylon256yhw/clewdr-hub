use tokio::sync::broadcast;
use tracing::warn;

use crate::{
    claude_code_state::probe::{probe_cookie, probe_oauth_account},
    config::{AccountSlot, AuthMethod},
    db::accounts::{AccountWithRuntime, active_reset_time, get_account_by_id},
    state::AdminEvent,
    stealth,
};

use super::state::{AccountPoolActor, AccountPoolState, RuntimeUpdate};

impl AccountPoolActor {
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
                // ApiKey accounts are pay-as-you-go and do not expose
                // subscription quota windows or OAuth profile metadata,
                // so the probe machinery (which is subscription-shaped)
                // does not apply. Clear the probing flag and return so
                // the spawn does not leave the account stuck in
                // `probing=true`.
                AuthMethod::ApiKey => {
                    let _ = handle.clear_probing(account_id).await;
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
    pub(super) fn build_cookie_probe_slot(
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
    pub(super) fn spawn_probes_for_unprobed(state: &mut AccountPoolState) {
        let unprobed = Self::bootstrap_probe_account_ids(state);
        for account_id in unprobed {
            Self::spawn_probe_guarded(state, account_id, None);
        }
    }

    /// Account IDs eligible for the bootstrap auto-probe. Extracted so the
    /// (auth_method == Cookie) ∧ (missing metadata) filter is unit-testable
    /// without standing up a real actor / spawning real probe tasks.
    pub(super) fn bootstrap_probe_account_ids(state: &AccountPoolState) -> Vec<i64> {
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
    pub(super) fn spawn_probe_accounts(
        state: &mut AccountPoolState,
        account_ids: &[i64],
        log_sink: Option<broadcast::Sender<AdminEvent>>,
    ) {
        for &account_id in account_ids {
            Self::spawn_probe_guarded(state, account_id, log_sink.clone());
        }
    }
}
