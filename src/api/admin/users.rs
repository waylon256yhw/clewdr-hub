use axum::{Json, extract::{Path, Query, State}, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::common::{Paginated, PaginationParams};
use crate::error::ClewdrError;
use crate::services::user_limiter::UserLimiterMap;

#[derive(Serialize, sqlx::FromRow)]
pub struct UserResponse {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub role: String,
    pub policy_id: i64,
    pub policy_name: String,
    pub disabled_at: Option<String>,
    pub last_seen_at: Option<String>,
    pub notes: Option<String>,
    pub key_count: i64,
    pub current_week_cost_nanousd: i64,
    pub current_month_cost_nanousd: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub display_name: Option<String>,
    pub password: Option<String>,
    pub role: Option<String>,
    pub policy_id: Option<i64>,
    pub notes: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub password: Option<String>,
    pub role: Option<String>,
    pub policy_id: Option<i64>,
    pub disabled: Option<bool>,
    pub notes: Option<String>,
}

fn current_week_start() -> String {
    use chrono::{Datelike, Utc};
    let now = Utc::now();
    let weekday = now.weekday().num_days_from_monday();
    let monday = now.date_naive() - chrono::Duration::days(weekday as i64);
    monday.format("%Y-%m-%d").to_string()
}

fn current_month_start() -> String {
    use chrono::Utc;
    Utc::now().format("%Y-%m-01").to_string()
}

fn user_list_query(week_start: &str, month_start: &str) -> String {
    format!(
        r#"SELECT u.id, u.username, u.display_name, u.role, u.policy_id,
               p.name as policy_name,
               u.disabled_at, u.last_seen_at, u.notes,
               (SELECT COUNT(*) FROM api_keys WHERE user_id = u.id) as key_count,
               COALESCE((SELECT cost_nanousd FROM usage_rollups
                         WHERE user_id = u.id AND period_type = 'week'
                         AND period_start = '{week_start}'), 0) as current_week_cost_nanousd,
               COALESCE((SELECT cost_nanousd FROM usage_rollups
                         WHERE user_id = u.id AND period_type = 'month'
                         AND period_start = '{month_start}'), 0) as current_month_cost_nanousd,
               u.created_at, u.updated_at
        FROM users u
        JOIN policies p ON u.policy_id = p.id"#
    )
}

pub async fn list(
    State(db): State<SqlitePool>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Paginated<UserResponse>>, ClewdrError> {
    let (offset, limit) = params.resolve();

    let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&db)
        .await?;

    let base = user_list_query(&current_week_start(), &current_month_start());
    let query = format!("{base} ORDER BY u.id LIMIT ?1 OFFSET ?2");
    let items: Vec<UserResponse> = sqlx::query_as(&query)
        .bind(limit)
        .bind(offset)
        .fetch_all(&db)
        .await?;

    Ok(Json(Paginated { items, total: total.0, offset, limit }))
}

pub async fn create(
    State(db): State<SqlitePool>,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserResponse>), ClewdrError> {
    let role = req.role.as_deref().unwrap_or("member");
    if role != "admin" && role != "member" {
        return Err(ClewdrError::BadRequest { msg: "role must be 'admin' or 'member'" });
    }
    if role == "admin" {
        match &req.password {
            None => return Err(ClewdrError::BadRequest { msg: "password is required for admin users" }),
            Some(pw) if pw.trim().is_empty() => return Err(ClewdrError::BadRequest { msg: "password cannot be empty" }),
            _ => {}
        }
    }

    let policy_id = req.policy_id.unwrap_or(1);
    let policy_exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM policies WHERE id = ?1")
        .bind(policy_id)
        .fetch_optional(&db)
        .await?;
    if policy_exists.is_none() {
        return Err(ClewdrError::BadRequest { msg: "policy_id does not exist" });
    }

    let password_hash = if let Some(ref pw) = req.password {
        if pw.trim().is_empty() {
            return Err(ClewdrError::BadRequest { msg: "password cannot be empty" });
        }
        let pw = pw.clone();
        let hash: String = tokio::task::spawn_blocking(move || crate::db::hash_password_public(&pw))
            .await
            .map_err(|_| ClewdrError::UnexpectedNone { msg: "password hash task panicked" })??;
        Some(hash)
    } else {
        None
    };

    let id = sqlx::query(
        "INSERT INTO users (username, display_name, password_hash, role, policy_id, notes) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&req.username)
    .bind(&req.display_name)
    .bind(&password_hash)
    .bind(role)
    .bind(policy_id)
    .bind(&req.notes)
    .execute(&db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref de) = e {
            if de.message().contains("UNIQUE") {
                return ClewdrError::Conflict { msg: "username already exists" };
            }
        }
        ClewdrError::from(e)
    })?
    .last_insert_rowid();

    let base = user_list_query(&current_week_start(), &current_month_start());
    let query = format!("{base} WHERE u.id = ?1");
    let row: UserResponse = sqlx::query_as(&query)
        .bind(id)
        .fetch_one(&db)
        .await?;

    Ok((StatusCode::CREATED, Json(row)))
}

pub async fn update(
    State(db): State<SqlitePool>,
    State(limiter): State<UserLimiterMap>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<UserResponse>, ClewdrError> {
    // Use a transaction for atomicity (prevents TOCTOU on admin checks + partial writes)
    let mut tx = db.begin().await?;

    let existing: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT role, password_hash FROM users WHERE id = ?1"
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((current_role, current_pw_hash)) = existing else {
        return Err(ClewdrError::NotFound { msg: "user not found" });
    };

    if let Some(ref username) = req.username {
        sqlx::query("UPDATE users SET username = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(username).bind(id).execute(&mut *tx).await
            .map_err(|e| {
                if let sqlx::Error::Database(ref de) = e {
                    if de.message().contains("UNIQUE") {
                        return ClewdrError::Conflict { msg: "username already exists" };
                    }
                }
                ClewdrError::from(e)
            })?;
    }
    if let Some(ref display_name) = req.display_name {
        sqlx::query("UPDATE users SET display_name = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(display_name).bind(id).execute(&mut *tx).await?;
    }
    if let Some(ref pw) = req.password {
        if pw.trim().is_empty() {
            return Err(ClewdrError::BadRequest { msg: "password cannot be empty" });
        }
        let pw = pw.clone();
        let hash: String = tokio::task::spawn_blocking(move || crate::db::hash_password_public(&pw))
            .await
            .map_err(|_| ClewdrError::UnexpectedNone { msg: "password hash task panicked" })??;
        sqlx::query("UPDATE users SET password_hash = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(&hash).bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE users SET session_version = session_version + 1 WHERE id = ?1")
            .bind(id).execute(&mut *tx).await?;
    }
    if let Some(ref role) = req.role {
        if role != "admin" && role != "member" {
            return Err(ClewdrError::BadRequest { msg: "role must be 'admin' or 'member'" });
        }
        if role == "admin" && current_pw_hash.is_none() && req.password.is_none() {
            return Err(ClewdrError::BadRequest { msg: "cannot promote to admin without a password" });
        }
        if current_role == "admin" && role == "member" {
            let admin_count: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled_at IS NULL AND id != ?1"
            ).bind(id).fetch_one(&mut *tx).await?;
            if admin_count.0 == 0 {
                return Err(ClewdrError::Conflict { msg: "cannot demote the last active admin" });
            }
        }
        sqlx::query("UPDATE users SET role = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(role).bind(id).execute(&mut *tx).await?;
    }
    if let Some(policy_id) = req.policy_id {
        let policy_exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM policies WHERE id = ?1")
            .bind(policy_id).fetch_optional(&mut *tx).await?;
        if policy_exists.is_none() {
            return Err(ClewdrError::BadRequest { msg: "policy_id does not exist" });
        }
        sqlx::query("UPDATE users SET policy_id = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(policy_id).bind(id).execute(&mut *tx).await?;
    }
    if let Some(disabled) = req.disabled {
        if disabled {
            // Check using "excluding self" count to be TOCTOU-safe within tx
            let admin_count: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled_at IS NULL AND id != ?1"
            ).bind(id).fetch_one(&mut *tx).await?;
            if current_role == "admin" && admin_count.0 == 0 {
                return Err(ClewdrError::Conflict { msg: "cannot disable the last active admin" });
            }
            sqlx::query("UPDATE users SET disabled_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP WHERE id = ?1")
                .bind(id).execute(&mut *tx).await?;
        } else {
            sqlx::query("UPDATE users SET disabled_at = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ?1")
                .bind(id).execute(&mut *tx).await?;
        }
    }
    if let Some(ref notes) = req.notes {
        sqlx::query("UPDATE users SET notes = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(notes).bind(id).execute(&mut *tx).await?;
    }

    tx.commit().await?;

    // Clean up limiter if user was disabled
    if req.disabled == Some(true) {
        limiter.remove(id).await;
    }

    let base = user_list_query(&current_week_start(), &current_month_start());
    let query = format!("{base} WHERE u.id = ?1");
    let row: UserResponse = sqlx::query_as(&query)
        .bind(id)
        .fetch_one(&db)
        .await?;

    Ok(Json(row))
}

pub async fn remove(
    State(db): State<SqlitePool>,
    State(limiter): State<UserLimiterMap>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ClewdrError> {
    // Transaction for atomic last-admin check + delete
    let mut tx = db.begin().await?;

    let role: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT role, disabled_at FROM users WHERE id = ?1"
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((role, disabled_at)) = role else {
        return Err(ClewdrError::NotFound { msg: "user not found" });
    };

    if role == "admin" && disabled_at.is_none() {
        let admin_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled_at IS NULL AND id != ?1"
        ).bind(id).fetch_one(&mut *tx).await?;
        if admin_count.0 == 0 {
            return Err(ClewdrError::Conflict { msg: "cannot delete the last active admin" });
        }
    }

    sqlx::query("DELETE FROM users WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    limiter.remove(id).await;

    Ok(StatusCode::NO_CONTENT)
}
