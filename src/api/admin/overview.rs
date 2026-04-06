use axum::{Extension, Json, extract::State};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::db::models::AuthenticatedUser;
use crate::error::ClewdrError;
use crate::services::cookie_actor::CookieActorHandle;
use crate::stealth;

#[derive(Serialize)]
pub struct OverviewResponse {
    pub version: String,
    pub server_time: String,
    pub cookies: CookieOverview,
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
pub struct CookieOverview {
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
    pub active: i64,
    pub disabled: i64,
}

#[derive(Serialize)]
pub struct StealthOverview {
    pub cli_version: String,
}

pub async fn overview(
    State(db): State<SqlitePool>,
    State(cookie_handle): State<CookieActorHandle>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<OverviewResponse>, ClewdrError> {
    let cookie_status = cookie_handle.get_status().await.ok();

    let cookies = cookie_status
        .map(|s| CookieOverview {
            valid: s.valid.len(),
            exhausted: s.exhausted.len(),
            invalid: s.invalid.len(),
        })
        .unwrap_or(CookieOverview {
            valid: 0,
            exhausted: 0,
            invalid: 0,
        });

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

    let account_stats: (i64, i64, i64) = sqlx::query_as(
        r#"SELECT COUNT(*),
                  COALESCE(SUM(CASE WHEN status = 'active' THEN 1 ELSE 0 END), 0),
                  COALESCE(SUM(CASE WHEN status = 'disabled' THEN 1 ELSE 0 END), 0)
           FROM accounts"#,
    )
    .fetch_one(&db)
    .await?;

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
        cookies,
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
            total: account_stats.0,
            active: account_stats.1,
            disabled: account_stats.2,
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
