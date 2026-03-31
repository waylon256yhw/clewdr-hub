use axum::{Json, extract::{Path, Query, State}, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::common::{Paginated, PaginationParams};
use crate::error::ClewdrError;
use crate::services::cookie_actor::CookieActorHandle;

#[derive(Serialize, sqlx::FromRow)]
pub struct AccountResponse {
    pub id: i64,
    pub name: String,
    pub rr_order: i64,
    pub max_slots: i64,
    pub status: String,
    pub organization_uuid: Option<String>,
    pub cooldown_until: Option<String>,
    pub cooldown_reason: Option<String>,
    pub last_refresh_at: Option<String>,
    pub last_used_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    pub rr_order: i64,
    pub max_slots: Option<i64>,
    pub cookie_blob: String,
    pub organization_uuid: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateAccountRequest {
    pub name: Option<String>,
    pub rr_order: Option<i64>,
    pub max_slots: Option<i64>,
    pub status: Option<String>,
    pub cookie_blob: Option<String>,
    pub organization_uuid: Option<String>,
}

const ACCOUNT_SELECT: &str = r#"
    SELECT id, name, rr_order, max_slots, status,
           organization_uuid, cooldown_until, cooldown_reason,
           last_refresh_at, last_used_at, last_error,
           created_at, updated_at
    FROM accounts
"#;

pub async fn list(
    State(db): State<SqlitePool>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Paginated<AccountResponse>>, ClewdrError> {
    let (offset, limit) = params.resolve();

    let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
        .fetch_one(&db).await?;

    let query = format!("{ACCOUNT_SELECT} ORDER BY rr_order LIMIT ?1 OFFSET ?2");
    let items: Vec<AccountResponse> = sqlx::query_as(&query)
        .bind(limit).bind(offset).fetch_all(&db).await?;

    Ok(Json(Paginated { items, total: total.0, offset, limit }))
}

pub async fn create(
    State(db): State<SqlitePool>,
    State(actor): State<CookieActorHandle>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<AccountResponse>), ClewdrError> {
    let max_slots = req.max_slots.unwrap_or(5);
    if max_slots <= 0 {
        return Err(ClewdrError::BadRequest { msg: "max_slots must be positive" });
    }

    let id = sqlx::query(
        "INSERT INTO accounts (name, rr_order, max_slots, cookie_blob, organization_uuid) VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(&req.name)
    .bind(req.rr_order)
    .bind(max_slots)
    .bind(&req.cookie_blob)
    .bind(&req.organization_uuid)
    .execute(&db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref de) = e {
            if de.message().contains("UNIQUE") {
                return ClewdrError::Conflict { msg: "account name or rr_order already exists" };
            }
        }
        ClewdrError::from(e)
    })?
    .last_insert_rowid();

    // Trigger actor reload so the new account is immediately available
    let _ = actor.reload_from_db().await;

    let query = format!("{ACCOUNT_SELECT} WHERE id = ?1");
    let row: AccountResponse = sqlx::query_as(&query)
        .bind(id).fetch_one(&db).await?;

    Ok((StatusCode::CREATED, Json(row)))
}

pub async fn update(
    State(db): State<SqlitePool>,
    State(actor): State<CookieActorHandle>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateAccountRequest>,
) -> Result<Json<AccountResponse>, ClewdrError> {
    if let Some(slots) = req.max_slots {
        if slots <= 0 { return Err(ClewdrError::BadRequest { msg: "max_slots must be positive" }); }
    }
    if let Some(ref status) = req.status {
        if !["active", "cooldown", "auth_error", "disabled"].contains(&status.as_str()) {
            return Err(ClewdrError::BadRequest { msg: "invalid status value" });
        }
    }

    let mut tx = db.begin().await?;

    let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM accounts WHERE id = ?1")
        .bind(id).fetch_optional(&mut *tx).await?;
    if exists.is_none() {
        return Err(ClewdrError::NotFound { msg: "account not found" });
    }

    if let Some(ref name) = req.name {
        sqlx::query("UPDATE accounts SET name = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(name).bind(id).execute(&mut *tx).await
            .map_err(|e| {
                if let sqlx::Error::Database(ref de) = e {
                    if de.message().contains("UNIQUE") {
                        return ClewdrError::Conflict { msg: "account name already exists" };
                    }
                }
                ClewdrError::from(e)
            })?;
    }
    if let Some(rr) = req.rr_order {
        sqlx::query("UPDATE accounts SET rr_order = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(rr).bind(id).execute(&mut *tx).await
            .map_err(|e| {
                if let sqlx::Error::Database(ref de) = e {
                    if de.message().contains("UNIQUE") {
                        return ClewdrError::Conflict { msg: "rr_order already exists" };
                    }
                }
                ClewdrError::from(e)
            })?;
    }
    if let Some(slots) = req.max_slots {
        sqlx::query("UPDATE accounts SET max_slots = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(slots).bind(id).execute(&mut *tx).await?;
    }
    if let Some(ref status) = req.status {
        sqlx::query("UPDATE accounts SET status = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(status).bind(id).execute(&mut *tx).await?;
    }
    if let Some(ref blob) = req.cookie_blob {
        sqlx::query("UPDATE accounts SET cookie_blob = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(blob).bind(id).execute(&mut *tx).await?;
    }
    if let Some(ref org) = req.organization_uuid {
        sqlx::query("UPDATE accounts SET organization_uuid = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(org).bind(id).execute(&mut *tx).await?;
    }

    tx.commit().await?;

    // Trigger actor reload
    let _ = actor.reload_from_db().await;

    let query = format!("{ACCOUNT_SELECT} WHERE id = ?1");
    let row: AccountResponse = sqlx::query_as(&query)
        .bind(id).fetch_one(&db).await?;

    Ok(Json(row))
}

pub async fn remove(
    State(db): State<SqlitePool>,
    State(actor): State<CookieActorHandle>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ClewdrError> {
    let result = sqlx::query("DELETE FROM accounts WHERE id = ?1")
        .bind(id).execute(&db).await?;

    if result.rows_affected() == 0 {
        return Err(ClewdrError::NotFound { msg: "account not found" });
    }

    // Trigger actor reload
    let _ = actor.reload_from_db().await;

    Ok(StatusCode::NO_CONTENT)
}
