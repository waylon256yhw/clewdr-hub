use axum::{Json, extract::State};
use serde::Serialize;

use crate::services::account_health::HealthDetail;
use crate::services::account_pool::AccountPoolHandle;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub accounts: AccountCounts,
    pub ready: bool,
}

/// Top-level `valid` / `exhausted` preserve the pre-Step-2.5 meaning:
/// the sizes of `state.valid` / `state.exhausted` in the pool. They are
/// *not* "instantly dispatchable" counters; saturated-inflight rows
/// still count toward `valid`. `detail` exposes the orthogonal slices
/// (`dispatchable_now` / `saturated` / …) for richer monitoring.
#[derive(Serialize)]
pub struct AccountCounts {
    pub valid: usize,
    pub exhausted: usize,
    pub detail: HealthDetail,
}

pub async fn health(State(pool_handle): State<AccountPoolHandle>) -> Json<HealthResponse> {
    let (valid, exhausted, detail) = match pool_handle.get_health_snapshot().await {
        Ok(snap) => (
            snap.summary.pool.valid,
            snap.summary.pool.exhausted,
            snap.summary.detail,
        ),
        Err(_) => (0, 0, HealthDetail::default()),
    };
    Json(HealthResponse {
        status: if valid > 0 { "ok" } else { "degraded" },
        accounts: AccountCounts {
            valid,
            exhausted,
            detail,
        },
        ready: valid > 0,
    })
}
