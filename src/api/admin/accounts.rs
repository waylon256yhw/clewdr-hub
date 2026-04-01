use axum::{Json, extract::{Path, Query, State}, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::common::{Paginated, PaginationParams};
use crate::db::accounts::{load_all_accounts, AccountWithRuntime};
use crate::error::ClewdrError;
use crate::services::cookie_actor::CookieActorHandle;

#[derive(Serialize)]
pub struct UsageWindowResponse {
    pub has_reset: Option<bool>,
    pub resets_at: Option<i64>,
    pub utilization: Option<f64>,
}

#[derive(Serialize)]
pub struct AccountRuntimeResponse {
    pub resets_last_checked_at: Option<i64>,
    pub session: Option<UsageWindowResponse>,
    pub weekly: Option<UsageWindowResponse>,
    pub weekly_sonnet: Option<UsageWindowResponse>,
    pub weekly_opus: Option<UsageWindowResponse>,
}

#[derive(Serialize)]
pub struct AccountResponse {
    pub id: i64,
    pub name: String,
    pub rr_order: i64,
    pub status: String,
    pub email: Option<String>,
    pub account_type: Option<String>,
    pub invalid_reason: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub runtime: Option<AccountRuntimeResponse>,
}

fn map_account(row: &AccountWithRuntime) -> AccountResponse {
    let runtime = row.runtime.as_ref().map(|rt| AccountRuntimeResponse {
        resets_last_checked_at: rt.resets_last_checked_at,
        session: Some(UsageWindowResponse {
            has_reset: rt.session_has_reset,
            resets_at: rt.session_resets_at,
            utilization: rt.session_utilization,
        }),
        weekly: Some(UsageWindowResponse {
            has_reset: rt.weekly_has_reset,
            resets_at: rt.weekly_resets_at,
            utilization: rt.weekly_utilization,
        }),
        weekly_sonnet: Some(UsageWindowResponse {
            has_reset: rt.weekly_sonnet_has_reset,
            resets_at: rt.weekly_sonnet_resets_at,
            utilization: rt.weekly_sonnet_utilization,
        }),
        weekly_opus: Some(UsageWindowResponse {
            has_reset: rt.weekly_opus_has_reset,
            resets_at: rt.weekly_opus_resets_at,
            utilization: rt.weekly_opus_utilization,
        }),
    });
    AccountResponse {
        id: row.id,
        name: row.name.clone(),
        rr_order: row.rr_order,
        status: row.status.clone(),
        email: row.email.clone(),
        account_type: row.account_type.clone(),
        invalid_reason: row.invalid_reason.clone(),
        created_at: None,
        updated_at: None,
        runtime,
    }
}

#[derive(Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    pub rr_order: Option<i64>,
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

pub async fn list(
    State(db): State<SqlitePool>,
    Query(_params): Query<PaginationParams>,
) -> Result<Json<Paginated<AccountResponse>>, ClewdrError> {
    let all = load_all_accounts(&db).await?;
    let total = all.len() as i64;
    let items: Vec<AccountResponse> = all.iter().map(map_account).collect();
    Ok(Json(Paginated { items, total, offset: 0, limit: total }))
}

pub async fn create(
    State(db): State<SqlitePool>,
    State(actor): State<CookieActorHandle>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ClewdrError> {
    let max_slots = req.max_slots.unwrap_or(5);
    if max_slots <= 0 {
        return Err(ClewdrError::BadRequest { msg: "max_slots must be positive" });
    }

    let rr_order = match req.rr_order {
        Some(v) => v,
        None => {
            let (max_rr,): (Option<i64>,) = sqlx::query_as("SELECT MAX(rr_order) FROM accounts")
                .fetch_one(&db)
                .await?;
            max_rr.unwrap_or(-1) + 1
        }
    };

    let id = sqlx::query(
        "INSERT INTO accounts (name, rr_order, max_slots, cookie_blob, organization_uuid) VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(&req.name)
    .bind(rr_order)
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

    let _ = actor.reload_from_db().await;

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

pub async fn update(
    State(db): State<SqlitePool>,
    State(actor): State<CookieActorHandle>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateAccountRequest>,
) -> Result<Json<serde_json::Value>, ClewdrError> {
    if let Some(slots) = req.max_slots {
        if slots <= 0 { return Err(ClewdrError::BadRequest { msg: "max_slots must be positive" }); }
    }
    if let Some(ref status) = req.status {
        if !["active", "disabled"].contains(&status.as_str()) {
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
        if status == "active" {
            sqlx::query("UPDATE accounts SET status = 'active', invalid_reason = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ?1")
                .bind(id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM account_runtime_state WHERE account_id = ?1")
                .bind(id).execute(&mut *tx).await?;
        } else {
            sqlx::query("UPDATE accounts SET status = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
                .bind(status).bind(id).execute(&mut *tx).await?;
        }
    }
    if let Some(ref blob) = req.cookie_blob {
        sqlx::query("UPDATE accounts SET email = NULL, account_type = NULL, organization_uuid = NULL, invalid_reason = NULL WHERE id = ?1")
            .bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM account_runtime_state WHERE account_id = ?1")
            .bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE accounts SET cookie_blob = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(blob).bind(id).execute(&mut *tx).await?;
    }
    if let Some(ref org) = req.organization_uuid {
        sqlx::query("UPDATE accounts SET organization_uuid = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(org).bind(id).execute(&mut *tx).await?;
    }

    tx.commit().await?;

    let _ = actor.reload_from_db().await;

    Ok(Json(serde_json::json!({ "ok": true })))
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

    let _ = actor.reload_from_db().await;

    Ok(StatusCode::NO_CONTENT)
}
