use std::collections::{HashMap, HashSet, VecDeque};

use moka::sync::Cache;
use ractor::{Actor, ActorProcessingErr, ActorRef};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::error;

use crate::{
    db::accounts::{batch_upsert_runtime_states, set_account_disabled},
    state::AdminEvent,
};

use super::state::{AccountPoolActor, AccountPoolMessage, AccountPoolState};

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
            AccountPoolMessage::UpdateCredentialIfCurrent(
                account_id,
                expected_refresh_token,
                token,
                reply_port,
            ) => {
                let matched = Self::pool_oauth_refresh_token_matches(
                    state,
                    account_id,
                    &expected_refresh_token,
                );
                if matched {
                    Self::update_slot_credential(state, account_id, token);
                }
                reply_port.send(matched)?;
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
            AccountPoolMessage::InvalidateIfCurrent(
                account_id,
                expected_refresh_token,
                reason,
                reply_port,
            ) => {
                let matched = Self::pool_oauth_refresh_token_matches(
                    state,
                    account_id,
                    &expected_refresh_token,
                );
                if matched {
                    Self::converge_invalidate(state, account_id, reason);
                }
                reply_port.send(matched)?;
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
