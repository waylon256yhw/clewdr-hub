use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::common::PaginationParams;
use crate::{
    db::accounts::{
        AccountWithRuntime, batch_upsert_runtime_states, get_account_by_id, load_all_accounts,
        update_account_metadata_unchecked, upsert_account_oauth,
    },
    error::ClewdrError,
    oauth::{AdminOAuthStartResponse, exchange_admin_oauth_callback, start_admin_oauth_flow},
    services::cookie_actor::CookieActorHandle,
};

#[derive(Serialize)]
pub struct AccountsListResponse {
    pub items: Vec<AccountResponse>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
    pub probing_ids: Vec<i64>,
    pub probe_errors: HashMap<i64, String>,
}

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
    pub auth_source: String,
    pub has_cookie: bool,
    pub has_oauth: bool,
    pub oauth_expires_at: Option<String>,
    pub last_refresh_at: Option<String>,
    pub last_error: Option<String>,
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
        auth_source: row.auth_source.clone(),
        has_cookie: row.cookie_blob.as_ref().is_some_and(|v| !v.is_empty()),
        has_oauth: row.oauth_token.is_some(),
        oauth_expires_at: row.oauth_expires_at.clone(),
        last_refresh_at: row.last_refresh_at.clone(),
        last_error: row.last_error.clone(),
        email: row.email.clone(),
        account_type: row.account_type.clone(),
        invalid_reason: row.invalid_reason.clone(),
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
        runtime,
    }
}

#[derive(Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    pub rr_order: Option<i64>,
    pub max_slots: Option<i64>,
    pub auth_source: Option<String>,
    pub cookie_blob: Option<String>,
    pub oauth_callback_input: Option<String>,
    pub oauth_state: Option<String>,
    pub organization_uuid: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateAccountRequest {
    pub name: Option<String>,
    pub rr_order: Option<i64>,
    pub max_slots: Option<i64>,
    pub status: Option<String>,
    pub auth_source: Option<String>,
    pub cookie_blob: Option<String>,
    pub oauth_callback_input: Option<String>,
    pub oauth_state: Option<String>,
    pub organization_uuid: Option<String>,
}

#[derive(Deserialize)]
pub struct StartOAuthRequest {
    pub redirect_uri: Option<String>,
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn derive_auth_source(
    requested: Option<&str>,
    has_cookie: bool,
    has_oauth: bool,
) -> Result<&'static str, ClewdrError> {
    let inferred = match (has_cookie, has_oauth) {
        (true, true) => "hybrid",
        (true, false) => "cookie",
        (false, true) => "oauth",
        (false, false) => {
            return Err(ClewdrError::BadRequest {
                msg: "Either cookie or OAuth callback input is required",
            });
        }
    };

    match requested {
        None => Ok(inferred),
        Some("cookie") if has_cookie => Ok("cookie"),
        Some("oauth") if has_oauth => Ok("oauth"),
        Some("hybrid") if has_cookie && has_oauth => Ok("hybrid"),
        Some("cookie" | "oauth" | "hybrid") => Err(ClewdrError::BadRequest {
            msg: "Requested auth_source does not match provided credentials",
        }),
        Some(_) => Err(ClewdrError::BadRequest {
            msg: "Invalid auth_source",
        }),
    }
}

pub async fn list(
    State(db): State<SqlitePool>,
    State(actor): State<CookieActorHandle>,
    Query(_params): Query<PaginationParams>,
) -> Result<Json<AccountsListResponse>, ClewdrError> {
    let all = load_all_accounts(&db).await?;
    let probing_ids = actor.get_probing_ids().await.unwrap_or_default();
    let probe_errors = actor.get_probe_errors().await.unwrap_or_default();
    let total = all.len() as i64;
    let items: Vec<AccountResponse> = all.iter().map(map_account).collect();
    Ok(Json(AccountsListResponse {
        items,
        total,
        offset: 0,
        limit: total,
        probing_ids,
        probe_errors,
    }))
}

pub async fn start_oauth(
    Json(req): Json<StartOAuthRequest>,
) -> Result<Json<AdminOAuthStartResponse>, ClewdrError> {
    Ok(Json(start_admin_oauth_flow(req.redirect_uri).await?))
}

pub async fn create(
    State(db): State<SqlitePool>,
    State(actor): State<CookieActorHandle>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ClewdrError> {
    let max_slots = req.max_slots.unwrap_or(5);
    if max_slots <= 0 {
        return Err(ClewdrError::BadRequest {
            msg: "max_slots must be positive",
        });
    }

    let cookie_blob = normalize_optional(req.cookie_blob);
    let oauth_state = normalize_optional(req.oauth_state);
    let oauth = match normalize_optional(req.oauth_callback_input) {
        Some(input) => Some(exchange_admin_oauth_callback(&input, oauth_state.as_deref()).await?),
        None => None,
    };
    let auth_source = derive_auth_source(
        req.auth_source.as_deref(),
        cookie_blob.is_some(),
        oauth.is_some(),
    )?;

    if let Some(ref cookie_blob) = cookie_blob {
        let dup: Option<(String,)> =
            sqlx::query_as("SELECT name FROM accounts WHERE cookie_blob = ?1")
                .bind(cookie_blob)
                .fetch_optional(&db)
                .await?;
        if dup.is_some() {
            return Err(ClewdrError::Conflict {
                msg: "该 Cookie 已被其他账号使用",
            });
        }
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
        "INSERT INTO accounts (
            name, rr_order, max_slots, status, auth_source, cookie_blob,
            oauth_access_token, oauth_refresh_token, oauth_expires_at,
            organization_uuid, last_refresh_at, last_error, email, account_type
        ) VALUES (?1, ?2, ?3, 'active', ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12)",
    )
    .bind(&req.name)
    .bind(rr_order)
    .bind(max_slots)
    .bind(auth_source)
    .bind(cookie_blob.as_deref())
    .bind(oauth.as_ref().map(|v| v.token.access_token.as_str()))
    .bind(oauth.as_ref().map(|v| v.token.refresh_token.as_str()))
    .bind(oauth.as_ref().map(|v| v.token.expires_at.to_rfc3339()))
    .bind(
        oauth
            .as_ref()
            .map(|v| v.snapshot.organization_uuid.as_str())
            .or(req.organization_uuid.as_deref()),
    )
    .bind(oauth.as_ref().map(|_| chrono::Utc::now().to_rfc3339()))
    .bind(oauth.as_ref().and_then(|v| v.snapshot.email.as_deref()))
    .bind(
        oauth
            .as_ref()
            .and_then(|v| v.snapshot.account_type.as_deref()),
    )
    .execute(&db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref de) = e {
            if de.message().contains("UNIQUE") {
                return ClewdrError::Conflict {
                    msg: "account name or rr_order already exists",
                };
            }
        }
        ClewdrError::from(e)
    })?
    .last_insert_rowid();

    if let Some(ref oauth) = oauth {
        batch_upsert_runtime_states(&db, &[(id, oauth.snapshot.runtime.clone())]).await?;
    }

    let _ = actor.reload_from_db().await;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

pub async fn update(
    State(db): State<SqlitePool>,
    State(actor): State<CookieActorHandle>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateAccountRequest>,
) -> Result<Json<serde_json::Value>, ClewdrError> {
    if let Some(slots) = req.max_slots
        && slots <= 0
    {
        return Err(ClewdrError::BadRequest {
            msg: "max_slots must be positive",
        });
    }
    if let Some(ref status) = req.status
        && !["active", "disabled", "auth_error"].contains(&status.as_str())
    {
        return Err(ClewdrError::BadRequest {
            msg: "invalid status value",
        });
    }

    let existing = get_account_by_id(&db, id)
        .await?
        .ok_or(ClewdrError::NotFound {
            msg: "account not found",
        })?;
    let new_cookie_blob = normalize_optional(req.cookie_blob.clone());
    let oauth_state = normalize_optional(req.oauth_state.clone());
    let oauth = match normalize_optional(req.oauth_callback_input.clone()) {
        Some(input) => Some(exchange_admin_oauth_callback(&input, oauth_state.as_deref()).await?),
        None => None,
    };
    let has_cookie = new_cookie_blob.is_some() || existing.cookie_blob.is_some();
    let has_oauth = oauth.is_some() || existing.oauth_token.is_some();
    let auth_source = derive_auth_source(req.auth_source.as_deref(), has_cookie, has_oauth)?;

    let mut tx = db.begin().await?;

    if let Some(ref name) = req.name {
        sqlx::query("UPDATE accounts SET name = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(name)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                if let sqlx::Error::Database(ref de) = e
                    && de.message().contains("UNIQUE")
                {
                    return ClewdrError::Conflict {
                        msg: "account name already exists",
                    };
                }
                ClewdrError::from(e)
            })?;
    }
    if let Some(rr) = req.rr_order {
        sqlx::query(
            "UPDATE accounts SET rr_order = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
        )
        .bind(rr)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref de) = e
                && de.message().contains("UNIQUE")
            {
                return ClewdrError::Conflict {
                    msg: "rr_order already exists",
                };
            }
            ClewdrError::from(e)
        })?;
    }
    if let Some(slots) = req.max_slots {
        sqlx::query(
            "UPDATE accounts SET max_slots = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
        )
        .bind(slots)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(ref status) = req.status {
        sqlx::query(
            "UPDATE accounts
             SET status = ?1,
                 invalid_reason = CASE WHEN ?1 = 'active' THEN NULL ELSE invalid_reason END,
                 last_error = CASE WHEN ?1 = 'active' THEN NULL ELSE last_error END,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2",
        )
        .bind(status)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(ref blob) = new_cookie_blob {
        let dup: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM accounts WHERE cookie_blob = ?1 AND id != ?2")
                .bind(blob)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if dup.is_some() {
            return Err(ClewdrError::Conflict {
                msg: "该 Cookie 已被其他账号使用",
            });
        }
        sqlx::query(
            "UPDATE accounts SET cookie_blob = ?1, invalid_reason = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
        )
        .bind(blob)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE accounts SET auth_source = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
    )
    .bind(auth_source)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if let Some(ref org) = req.organization_uuid {
        sqlx::query(
            "UPDATE accounts SET organization_uuid = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
        )
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    if let Some(ref oauth) = oauth {
        upsert_account_oauth(&db, id, Some(&oauth.token), None).await?;
        update_account_metadata_unchecked(
            &db,
            id,
            oauth.snapshot.email.as_deref(),
            oauth.snapshot.account_type.as_deref(),
            Some(oauth.snapshot.organization_uuid.as_str()),
        )
        .await?;
        batch_upsert_runtime_states(&db, &[(id, oauth.snapshot.runtime.clone())]).await?;
    }

    let _ = actor.reload_from_db().await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn remove(
    State(db): State<SqlitePool>,
    State(actor): State<CookieActorHandle>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ClewdrError> {
    let result = sqlx::query("DELETE FROM accounts WHERE id = ?1")
        .bind(id)
        .execute(&db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ClewdrError::NotFound {
            msg: "account not found",
        });
    }

    let _ = actor.reload_from_db().await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn probe_all(
    State(actor): State<CookieActorHandle>,
) -> Result<Json<serde_json::Value>, ClewdrError> {
    let ids = actor.probe_all().await?;
    Ok(Json(serde_json::json!({ "probing_ids": ids })))
}
