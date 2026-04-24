use axum::{Extension, Json, extract::State};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::db::models::AuthenticatedUser;
use crate::error::ClewdrError;
use crate::services::account_health::{
    AccountHealthState, AuthSourceCounts, HealthDetail, InvalidBreakdown, InvalidKind,
};
use crate::services::account_pool::AccountPoolHandle;
use crate::stealth;

#[derive(Serialize)]
pub struct OverviewResponse {
    pub version: String,
    pub server_time: String,
    pub pool: PoolOverview,
    pub users: UserOverview,
    pub api_keys: KeyOverview,
    pub accounts: AccountOverview,
    pub policies: i64,
    pub requests_1h: i64,
    pub requests_24h: i64,
    pub stealth: StealthOverview,
    pub must_change_password: bool,
}

/// Pool-level counts.
///
/// `valid` / `exhausted` / `invalid` are the legacy three-bucket view kept
/// for wire compatibility with older dashboards. `detail` exposes the
/// orthogonal diagnostic slices introduced in Step 2.5 (see
/// `AccountHealthSummary`). `invalid_breakdown` splits `invalid` by
/// `Reason` so admins can tell free / banned / disabled / … apart at a
/// glance.
#[derive(Serialize)]
pub struct PoolOverview {
    pub valid: usize,
    pub exhausted: usize,
    pub invalid: usize,
    pub detail: HealthDetail,
    pub invalid_breakdown: InvalidBreakdown,
}

#[derive(Serialize)]
pub struct UserOverview {
    pub total: i64,
    pub admins: i64,
    pub members: i64,
    pub disabled: i64,
}

#[derive(Serialize)]
pub struct KeyOverview {
    pub total: i64,
    pub active: i64,
    pub disabled: i64,
}

#[derive(Serialize)]
pub struct AccountOverview {
    pub total: i64,
    pub statuses: AccountStatusOverview,
    pub auth_sources: AccountAuthSourceOverview,
}

#[derive(Default, Serialize)]
pub struct AccountStatusOverview {
    pub active: i64,
    pub cooling: i64,
    pub error: i64,
    pub disabled: i64,
}

#[derive(Serialize)]
pub struct AccountAuthSourceOverview {
    pub oauth: i64,
    pub cookie: i64,
}

#[derive(Serialize)]
pub struct StealthOverview {
    pub cli_version: String,
}

pub async fn overview(
    State(db): State<SqlitePool>,
    State(actor): State<AccountPoolHandle>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<OverviewResponse>, ClewdrError> {
    let snapshot = actor.get_health_snapshot().await?;
    let summary = &snapshot.summary;

    let pool = PoolOverview {
        valid: summary.pool.valid,
        exhausted: summary.pool.exhausted,
        invalid: summary.pool.invalid,
        detail: summary.detail,
        invalid_breakdown: summary.invalid_breakdown,
    };

    // Aggregate statuses by health state so admin overview shares its
    // categorisation with /health and the accounts list. Unconfigured rows
    // stay in `active` to preserve the pre-Step-2.5 meaning of this counter
    // ("DB status=active, not cooling, not invalid").
    let mut statuses = AccountStatusOverview::default();
    for health in snapshot.per_account.values() {
        match &health.state {
            AccountHealthState::Active | AccountHealthState::Unconfigured => statuses.active += 1,
            AccountHealthState::CoolingDown { .. } => statuses.cooling += 1,
            AccountHealthState::Invalid {
                kind: InvalidKind::AuthError,
                ..
            } => statuses.error += 1,
            AccountHealthState::Invalid {
                kind: InvalidKind::Disabled,
                ..
            } => statuses.disabled += 1,
        }
    }

    let user_stats: (i64, i64, i64, i64) = sqlx::query_as(
        r#"SELECT COUNT(*),
                  COALESCE(SUM(CASE WHEN role = 'admin' THEN 1 ELSE 0 END), 0),
                  COALESCE(SUM(CASE WHEN role = 'member' THEN 1 ELSE 0 END), 0),
                  COALESCE(SUM(CASE WHEN disabled_at IS NOT NULL THEN 1 ELSE 0 END), 0)
           FROM users"#,
    )
    .fetch_one(&db)
    .await?;

    let key_stats: (i64, i64, i64) = sqlx::query_as(
        r#"SELECT COUNT(*),
                  COALESCE(SUM(CASE WHEN disabled_at IS NULL AND (expires_at IS NULL OR expires_at > CURRENT_TIMESTAMP) THEN 1 ELSE 0 END), 0),
                  COALESCE(SUM(CASE WHEN disabled_at IS NOT NULL THEN 1 ELSE 0 END), 0)
           FROM api_keys"#,
    ).fetch_one(&db).await?;

    let (policy_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM policies")
        .fetch_one(&db)
        .await?;

    let now = chrono::Utc::now();
    let one_hour_ago = (now - chrono::Duration::hours(1)).to_rfc3339();
    let one_day_ago = (now - chrono::Duration::hours(24)).to_rfc3339();

    let (requests_1h,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM request_logs WHERE started_at >= ?1")
            .bind(&one_hour_ago)
            .fetch_one(&db)
            .await?;
    let (requests_24h,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM request_logs WHERE started_at >= ?1")
            .bind(&one_day_ago)
            .fetch_one(&db)
            .await?;

    let profile = stealth::global_profile().load();

    let (must_change,): (i32,) =
        sqlx::query_as("SELECT must_change_password FROM users WHERE id = ?1")
            .bind(user.user_id)
            .fetch_one(&db)
            .await?;

    let AuthSourceCounts { oauth, cookie } = summary.auth_sources;
    Ok(Json(OverviewResponse {
        version: crate::VERSION_INFO.clone(),
        server_time: now.to_rfc3339(),
        pool,
        users: UserOverview {
            total: user_stats.0,
            admins: user_stats.1,
            members: user_stats.2,
            disabled: user_stats.3,
        },
        api_keys: KeyOverview {
            total: key_stats.0,
            active: key_stats.1,
            disabled: key_stats.2,
        },
        accounts: AccountOverview {
            total: summary.total,
            statuses,
            auth_sources: AccountAuthSourceOverview { oauth, cookie },
        },
        policies: policy_count,
        requests_1h,
        requests_24h,
        stealth: StealthOverview {
            cli_version: profile.cli_version.clone(),
        },
        must_change_password: must_change != 0,
    }))
}
