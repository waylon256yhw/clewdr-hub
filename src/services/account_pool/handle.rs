use std::collections::HashMap;

use chrono::Utc;
use ractor::{Actor, ActorRef};
use snafu::{GenerateImplicitData, Location};
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use crate::{
    config::{AccountSlot, Reason, TokenInfo},
    db::accounts::load_all_accounts,
    error::ClewdrError,
    services::account_health::{AccountHealthSnapshot, compose_health_snapshot},
    state::AdminEvent,
};

use super::{
    CredentialFingerprint,
    state::{
        AccountPoolActor, AccountPoolMessage, AccountPoolStatus, RuntimeMergeMode, RuntimeUpdate,
    },
};

const INTERVAL: u64 = 300;
const FLUSH_INTERVAL: u64 = 15;

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

    /// Update a refreshed OAuth token only if the pool still carries the
    /// credential that produced it. This closes the DB-CAS-to-pool-sync
    /// window when an admin rotation and its reload race a refresh result.
    pub async fn update_credential_if_current(
        &self,
        account_id: i64,
        expected_refresh_token: &str,
        token: Option<TokenInfo>,
    ) -> Result<bool, ClewdrError> {
        ractor::call!(
            self.actor_ref,
            AccountPoolMessage::UpdateCredentialIfCurrent,
            account_id,
            expected_refresh_token.to_string(),
            token
        )
        .map_err(|e| ClewdrError::RactorError {
            loc: Location::generate(),
            msg: format!("Failed to conditionally update credential: {e}"),
        })
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

    /// Invalidate an OAuth slot only if it still represents the credential
    /// whose terminal failure was committed to DB.
    pub async fn invalidate_if_current(
        &self,
        account_id: i64,
        expected_refresh_token: &str,
        reason: Reason,
    ) -> Result<bool, ClewdrError> {
        ractor::call!(
            self.actor_ref,
            AccountPoolMessage::InvalidateIfCurrent,
            account_id,
            expected_refresh_token.to_string(),
            reason
        )
        .map_err(|e| ClewdrError::RactorError {
            loc: Location::generate(),
            msg: format!("Failed to conditionally invalidate credential: {e}"),
        })
    }
}
