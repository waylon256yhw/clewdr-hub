use axum::{Extension, Json, extract::State};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::db::accounts::{load_all_accounts, summarize_accounts};
use crate::db::models::AuthenticatedUser;
use crate::error::ClewdrError;
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

#[derive(Serialize)]
pub struct PoolOverview {
    pub valid: usize,
    pub exhausted: usize,
    pub invalid: usize,
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

#[derive(Serialize)]
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
    pub hybrid: i64,
}

#[derive(Serialize)]
pub struct StealthOverview {
    pub cli_version: String,
}

pub async fn overview(
    State(db): State<SqlitePool>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<OverviewResponse>, ClewdrError> {
    let accounts = load_all_accounts(&db).await?;
    let account_summary = summarize_accounts(&accounts);
    let pool = PoolOverview {
        valid: account_summary.pool.valid,
        exhausted: account_summary.pool.exhausted,
        invalid: account_summary.pool.invalid,
    };

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
            total: account_summary.total,
            statuses: AccountStatusOverview {
                active: account_summary.statuses.active,
                cooling: account_summary.statuses.cooling,
                error: account_summary.statuses.error,
                disabled: account_summary.statuses.disabled,
            },
            auth_sources: AccountAuthSourceOverview {
                oauth: account_summary.auth_sources.oauth,
                cookie: account_summary.auth_sources.cookie,
                hybrid: account_summary.auth_sources.hybrid,
            },
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
