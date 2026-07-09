use colored::Colorize;
use tracing::info;

use crate::services::account_health::{AccountHealthSummary, PoolSnapshotView};

use super::state::{AccountPoolActor, AccountPoolState, AccountPoolStatus};

impl AccountPoolActor {
    pub(super) fn log(state: &AccountPoolState) {
        info!(
            "Valid: {}, Exhausted: {}, Invalid: {}",
            state.valid.len().to_string().green(),
            state.exhausted.len().to_string().yellow(),
            state.invalid.len().to_string().red(),
        );
    }

    pub(super) fn log_account_summary(summary: &AccountHealthSummary) {
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

    pub(super) fn report(state: &AccountPoolState) -> AccountPoolStatus {
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
    pub(super) fn snapshot_view(state: &AccountPoolState) -> PoolSnapshotView {
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
            reactivated: state.reactivated.clone(),
            probe_errors: state.probe_errors.clone(),
        }
    }
}
