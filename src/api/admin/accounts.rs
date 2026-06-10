use std::collections::BTreeMap;
use std::str::FromStr;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use http::header::USER_AGENT;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use sqlx::{Executor, Sqlite, SqlitePool};

use super::common::{PaginationParams, normalize_optional};
use crate::{
    billing::{BillingContext, RequestType, persist_probe_log},
    claude_code_state::{
        ClaudeCodeState, build_api_client, is_reserved_api_key_extra_header,
        normalize_api_key_base_url,
    },
    config::{AccountSlot, AuthMethod, CLAUDE_ENDPOINT, ClewdrCookie},
    db::accounts::{
        AccountWithRuntime, batch_upsert_runtime_states, clear_account_cooldown,
        find_account_by_organization_uuid, get_account_by_id, load_all_accounts,
        set_account_active, set_account_auth_error, set_account_disabled, set_account_last_failure,
        set_account_reset_time, update_account_metadata_unchecked, upsert_account_oauth,
    },
    db::proxies::{build_proxy_url, get_proxy_by_id},
    error::{
        ClewdrError, WreqSnafu, claude_error_from_response_parts, display_account_invalid_reason,
        sanitize_account_error_message,
    },
    oauth::{
        AdminOAuthStartResponse, exchange_admin_oauth_callback, refresh_oauth_token,
        start_admin_oauth_flow,
    },
    services::account_pool::AccountPoolHandle,
    services::{
        account_error::{
            AccountFailureAction, AccountFailureContext, AccountFailureContextPersisted,
            FailureSource, classify_account_failure,
        },
        account_health::AccountHealth,
    },
    state::AppState,
    stealth::SharedStealthProfile,
};

#[derive(Serialize)]
pub struct AccountsListResponse {
    pub items: Vec<AccountResponse>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

#[derive(Serialize)]
pub struct UsageWindowResponse {
    pub has_reset: Option<bool>,
    pub resets_at: Option<i64>,
    pub utilization: Option<f64>,
}

#[derive(Serialize)]
pub struct AccountRuntimeResponse {
    pub reset_time: Option<i64>,
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
    pub proxy_id: Option<i64>,
    pub proxy_name: Option<String>,
    pub drain_first: bool,
    pub status: String,
    pub auth_source: String,
    pub has_cookie: bool,
    pub has_oauth: bool,
    /// True iff the row's `auth_source = 'api_key'` and the row has both
    /// `api_key_base_url` and `api_key_secret` populated. The secret
    /// itself is NEVER echoed back.
    pub has_api_key: bool,
    /// Normalized base URL. Safe to echo (admin entered it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_base_url: Option<String>,
    /// Per-account extra headers attached to every ApiKey send.
    /// These are admin-configured routing headers, not the ApiKey
    /// secret itself, so the admin API echoes them for edit forms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_extra_headers: Option<BTreeMap<String, String>>,
    /// All-time billable Messages spend attributed to this account.
    pub total_cost_nanousd: i64,
    pub oauth_expires_at: Option<String>,
    pub last_refresh_at: Option<String>,
    pub last_error: Option<String>,
    pub email: Option<String>,
    pub account_type: Option<String>,
    /// e.g. `default_claude_max_20x`. Lets the frontend render the
    /// 5x / 20x distinction instead of the coarser `account_type`.
    pub rate_limit_tier: Option<String>,
    /// RFC3339 anchor for the renewal-countdown UI. Origin differs by
    /// auth source: OAuth populates from
    /// `profile.organization.subscription_created_at`; Cookie falls
    /// back to the org's `created_at` (see BootstrapInfo for details).
    pub subscription_created_at: Option<String>,
    /// e.g. `google_play_subscription`, `stripe`. Tooltip-only on the
    /// AccountCard; informational.
    pub billing_type: Option<String>,
    pub invalid_reason: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub runtime: Option<AccountRuntimeResponse>,
    /// Unified account-health view merging the DB row with the current
    /// pool state. `None` when the account has not been indexed by the
    /// pool yet (e.g., just created between snapshot and list load).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<AccountHealth>,
}

fn map_account(row: &AccountWithRuntime, health: Option<AccountHealth>) -> AccountResponse {
    let runtime = row.runtime.as_ref().map(|rt| AccountRuntimeResponse {
        reset_time: rt.reset_time,
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
        proxy_id: row.proxy_id,
        proxy_name: row.proxy_name.clone(),
        drain_first: row.drain_first,
        status: row.status.clone(),
        auth_source: row.auth_source.clone(),
        has_cookie: row.cookie_blob.as_ref().is_some_and(|v| !v.is_empty()),
        has_oauth: row.oauth_token.is_some(),
        has_api_key: row.auth_source == "api_key"
            && row
                .api_key_base_url
                .as_deref()
                .is_some_and(|s| !s.is_empty())
            && row.api_key_secret.as_deref().is_some_and(|s| !s.is_empty()),
        api_key_base_url: if row.auth_source == "api_key" {
            row.api_key_base_url.clone()
        } else {
            None
        },
        // Parse defensively: a malformed JSON column shouldn't 500 the
        // accounts list — surface as "no extra headers" instead.
        api_key_extra_headers: row
            .api_key_extra_headers
            .as_deref()
            .and_then(|s| serde_json::from_str::<BTreeMap<String, String>>(s).ok())
            .filter(|headers| !headers.is_empty()),
        total_cost_nanousd: row.total_cost_nanousd,
        oauth_expires_at: row.oauth_expires_at.clone(),
        last_refresh_at: row.last_refresh_at.clone(),
        last_error: row
            .last_error
            .as_deref()
            .map(sanitize_account_error_message),
        email: row.email.clone(),
        account_type: row.account_type.clone(),
        rate_limit_tier: row.rate_limit_tier.clone(),
        subscription_created_at: row.subscription_created_at.clone(),
        billing_type: row.billing_type.clone(),
        invalid_reason: row
            .invalid_reason
            .as_deref()
            .map(display_account_invalid_reason),
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
        runtime,
        health,
    }
}

#[derive(Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    pub rr_order: Option<i64>,
    pub max_slots: Option<i64>,
    pub proxy_id: Option<i64>,
    pub drain_first: Option<bool>,
    pub auth_source: Option<String>,
    pub cookie_blob: Option<String>,
    pub oauth_callback_input: Option<String>,
    pub oauth_state: Option<String>,
    pub organization_uuid: Option<String>,
    /// ApiKey credential — base URL (e.g. `https://api.anthropic.com/`),
    /// API key, and optional extra HTTP headers attached to every send.
    /// All three are stored denormalized in the `accounts` row.
    #[serde(default)]
    pub api_key_base_url: Option<String>,
    #[serde(default)]
    pub api_key_secret: Option<String>,
    /// `Some({})` is explicit "no headers"; `None` is "not provided".
    /// On create both behave the same — there is no existing value to
    /// preserve. On update they differ (see UpdateAccountRequest).
    #[serde(default)]
    pub api_key_extra_headers: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
pub struct UpdateAccountRequest {
    pub name: Option<String>,
    pub rr_order: Option<i64>,
    pub max_slots: Option<i64>,
    pub proxy_id: Option<i64>,
    pub drain_first: Option<bool>,
    pub status: Option<String>,
    pub auth_source: Option<String>,
    pub cookie_blob: Option<String>,
    pub oauth_callback_input: Option<String>,
    pub oauth_state: Option<String>,
    pub organization_uuid: Option<String>,
    /// On update, semantics mirror cookie/oauth: empty / omitted means
    /// "keep existing"; a non-empty value replaces. Switching INTO
    /// `api_key` from cookie/oauth requires both `base_url` and
    /// `secret` to be present (a `Some("")` is treated as omitted).
    #[serde(default)]
    pub api_key_base_url: Option<String>,
    #[serde(default)]
    pub api_key_secret: Option<String>,
    /// Tri-state:
    ///   - `None` / omitted → keep existing headers
    ///   - `Some({})` → explicit clear (NULL the column)
    ///   - `Some(map)` non-empty → replace existing headers
    #[serde(default)]
    pub api_key_extra_headers: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
pub struct StartOAuthRequest {
    pub redirect_uri: Option<String>,
}

// Parse a user-supplied cookie into its canonical inner form
// (sk-ant-sid...AA), so downstream comparisons and the stale-write guard in
// update_account_metadata stay consistent with what ClewdrCookie::from_str
// produces when the pool is (re)loaded.
fn normalize_cookie_blob(value: Option<String>) -> Result<Option<String>, ClewdrError> {
    let Some(trimmed) = normalize_optional(value) else {
        return Ok(None);
    };
    let parsed = ClewdrCookie::from_str(&trimmed).map_err(|_| ClewdrError::BadRequest {
        msg: "cookie format invalid",
    })?;
    Ok(Some((*parsed).to_owned()))
}

async fn reject_duplicate_oauth_identity<'e, E>(
    executor: E,
    organization_uuid: &str,
    excluded_account_id: Option<i64>,
) -> Result<(), ClewdrError>
where
    E: Executor<'e, Database = Sqlite>,
{
    if let Some(conflict) =
        find_account_by_organization_uuid(executor, organization_uuid, excluded_account_id).await?
    {
        return Err(ClewdrError::ConflictMessage {
            msg: format!(
                "该 OAuth 账号已被账号 #{} ({}) 使用",
                conflict.id, conflict.name
            ),
        });
    }
    Ok(())
}

async fn resolve_proxy_url(
    db: &SqlitePool,
    proxy_id: Option<i64>,
) -> Result<Option<(i64, String)>, ClewdrError> {
    let Some(proxy_id) = proxy_id.filter(|id| *id > 0) else {
        return Ok(None);
    };
    let proxy = get_proxy_by_id(db, proxy_id)
        .await?
        .ok_or(ClewdrError::NotFound {
            msg: "proxy not found",
        })?;
    let url = build_proxy_url(&proxy).map_err(|_| ClewdrError::BadRequest {
        msg: "Invalid proxy configuration",
    })?;
    Ok(Some((proxy_id, url)))
}

fn derive_auth_source(
    requested: Option<&str>,
    submitted_cookie: bool,
    submitted_oauth: bool,
    submitted_api_key: bool,
    existing: Option<&str>,
) -> Result<&'static str, ClewdrError> {
    let submitted_count = submitted_cookie as u8 + submitted_oauth as u8 + submitted_api_key as u8;
    if submitted_count > 1 {
        return Err(ClewdrError::BadRequest {
            msg: "Submit exactly one credential kind: cookie, OAuth callback, or API key",
        });
    }
    let derived: &'static str = if submitted_cookie {
        "cookie"
    } else if submitted_oauth {
        "oauth"
    } else if submitted_api_key {
        "api_key"
    } else {
        match existing {
            Some("cookie") => "cookie",
            Some("oauth") => "oauth",
            Some("api_key") => "api_key",
            _ => {
                return Err(ClewdrError::BadRequest {
                    msg: "Either cookie, OAuth callback, or API key is required",
                });
            }
        }
    };

    match requested {
        None => Ok(derived),
        Some(r) if r == derived => Ok(derived),
        Some("cookie" | "oauth" | "api_key") => Err(ClewdrError::BadRequest {
            msg: "Requested auth_source does not match provided credentials",
        }),
        Some(_) => Err(ClewdrError::BadRequest {
            msg: "Invalid auth_source",
        }),
    }
}

/// Validate a caller-supplied `api_key_extra_headers` map against the
/// reserved-name list shared with the send-side filter
/// (`is_reserved_api_key_extra_header`). Empty keys are also rejected
/// — they cannot map to a valid HTTP header.
///
/// This is the primary user-facing guard; the send-time filter in
/// `chat::execute_api_key_request` repeats the check as defense in
/// depth for the case of a manual DB edit that bypasses validation.
fn validate_api_key_extra_headers(map: &BTreeMap<String, String>) -> Result<(), ClewdrError> {
    for key in map.keys() {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Err(ClewdrError::BadRequest {
                msg: "api_key_extra_headers contains an empty key",
            });
        }
        if is_reserved_api_key_extra_header(trimmed) {
            return Err(ClewdrError::BadRequestMessage {
                msg: format!(
                    "api_key_extra_headers key '{trimmed}' is reserved (would shadow a header the ApiKey dispatch sets itself or the transport owns)"
                ),
            });
        }
    }
    Ok(())
}

/// Serialize an extra-headers map to the JSON string we store in the
/// `accounts.api_key_extra_headers` column. Returns `None` for an
/// empty map (column should be NULL, not `"{}"`, so the loader and
/// the `ApiKeyExtraHeaders::is_empty` check agree).
fn extra_headers_to_db(map: &BTreeMap<String, String>) -> Option<String> {
    if map.is_empty() {
        None
    } else {
        // serde_json::to_string on BTreeMap<String,String> is infallible.
        Some(serde_json::to_string(map).expect("BTreeMap serialization is infallible"))
    }
}

pub async fn list(
    State(db): State<SqlitePool>,
    State(actor): State<AccountPoolHandle>,
    Query(_params): Query<PaginationParams>,
) -> Result<Json<AccountsListResponse>, ClewdrError> {
    let all = load_all_accounts(&db).await?;
    // Single snapshot drives each item's `health`, so the list cannot
    // disagree with itself about probing state or the last probe error.
    let snapshot = actor.get_health_snapshot().await?;
    let total = all.len() as i64;
    let items: Vec<AccountResponse> = all
        .iter()
        .map(|row| map_account(row, snapshot.per_account.get(&row.id).cloned()))
        .collect();
    Ok(Json(AccountsListResponse {
        items,
        total,
        offset: 0,
        limit: total,
    }))
}

pub async fn start_oauth(
    Json(req): Json<StartOAuthRequest>,
) -> Result<Json<AdminOAuthStartResponse>, ClewdrError> {
    Ok(Json(start_admin_oauth_flow(req.redirect_uri).await?))
}

pub async fn create(
    State(db): State<SqlitePool>,
    State(actor): State<AccountPoolHandle>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ClewdrError> {
    let max_slots = req.max_slots.unwrap_or(5);
    if max_slots <= 0 {
        return Err(ClewdrError::BadRequest {
            msg: "max_slots must be positive",
        });
    }

    let proxy_binding = resolve_proxy_url(&db, req.proxy_id).await?;
    let cookie_blob = normalize_cookie_blob(req.cookie_blob)?;
    let oauth_state = normalize_optional(req.oauth_state);
    let oauth_callback_input = normalize_optional(req.oauth_callback_input);

    // ApiKey inputs: normalize the base URL and validate extra-header
    // names BEFORE any DB work or OAuth round-trip, so a clean 400
    // surfaces fast on bad input. Empty / whitespace-only base_url and
    // secret are treated as "not submitted" (same convention as
    // cookie_blob via normalize_optional).
    let raw_api_key_base_url = normalize_optional(req.api_key_base_url);
    let raw_api_key_secret = normalize_optional(req.api_key_secret);
    let api_key_extra_headers_payload = req.api_key_extra_headers;
    let submitting_api_key = raw_api_key_base_url.is_some()
        || raw_api_key_secret.is_some()
        || api_key_extra_headers_payload.is_some();

    let api_key_base_url_normalized: Option<String> = match raw_api_key_base_url.as_deref() {
        Some(raw) => Some(normalize_api_key_base_url(raw)?.as_str().to_string()),
        None => None,
    };
    if let Some(ref map) = api_key_extra_headers_payload {
        validate_api_key_extra_headers(map)?;
    }
    let api_key_extra_headers_json: Option<String> = api_key_extra_headers_payload
        .as_ref()
        .and_then(extra_headers_to_db);

    let submitted_count = cookie_blob.is_some() as u8
        + oauth_callback_input.is_some() as u8
        + submitting_api_key as u8;
    if submitted_count > 1 {
        return Err(ClewdrError::BadRequest {
            msg: "Submit exactly one credential kind: cookie, OAuth callback, or API key",
        });
    }
    let oauth = match oauth_callback_input {
        Some(input) => Some(
            exchange_admin_oauth_callback(
                &input,
                oauth_state.as_deref(),
                proxy_binding.as_ref().map(|(_, url)| url.as_str()),
            )
            .await?,
        ),
        None => None,
    };
    let auth_source = derive_auth_source(
        req.auth_source.as_deref(),
        cookie_blob.is_some(),
        oauth.is_some(),
        submitting_api_key,
        None,
    )?;

    // For api_key, both base_url and secret are mandatory on create
    // (the schema mutex CHECK would reject a partial row anyway, but
    // surface it as a clean 400 instead of a SQLite constraint error).
    if auth_source == "api_key" {
        if api_key_base_url_normalized.is_none() {
            return Err(ClewdrError::BadRequest {
                msg: "api_key_base_url is required for api_key accounts",
            });
        }
        if raw_api_key_secret.is_none() {
            return Err(ClewdrError::BadRequest {
                msg: "api_key_secret is required for api_key accounts",
            });
        }
    }

    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;

    if let Some(ref cookie_blob) = cookie_blob {
        let dup: Option<(String,)> =
            sqlx::query_as("SELECT name FROM accounts WHERE cookie_blob = ?1")
                .bind(cookie_blob)
                .fetch_optional(&mut *tx)
                .await?;
        if dup.is_some() {
            return Err(ClewdrError::Conflict {
                msg: "该 Cookie 已被其他账号使用",
            });
        }
    }
    if let Some(ref oauth) = oauth {
        reject_duplicate_oauth_identity(&mut *tx, &oauth.snapshot.organization_uuid, None).await?;
    }

    let rr_order = match req.rr_order {
        Some(v) => v,
        None => {
            let (max_rr,): (Option<i64>,) = sqlx::query_as("SELECT MAX(rr_order) FROM accounts")
                .fetch_one(&mut *tx)
                .await?;
            max_rr.unwrap_or(-1) + 1
        }
    };

    let id = sqlx::query(
        "INSERT INTO accounts (
            name, rr_order, max_slots, proxy_id, status, auth_source, cookie_blob,
            oauth_access_token, oauth_refresh_token, oauth_expires_at,
            organization_uuid, last_refresh_at, last_error, email, account_type,
            rate_limit_tier, subscription_created_at, billing_type,
            drain_first,
            api_key_base_url, api_key_secret, api_key_extra_headers
        ) VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
    )
    .bind(&req.name)
    .bind(rr_order)
    .bind(max_slots)
    .bind(proxy_binding.as_ref().map(|(id, _)| *id))
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
    .bind(
        oauth
            .as_ref()
            .and_then(|v| v.snapshot.rate_limit_tier.as_deref()),
    )
    .bind(
        oauth
            .as_ref()
            .and_then(|v| v.snapshot.subscription_created_at.as_deref()),
    )
    .bind(
        oauth
            .as_ref()
            .and_then(|v| v.snapshot.billing_type.as_deref()),
    )
    .bind(req.drain_first.unwrap_or(false) as i64)
    .bind(api_key_base_url_normalized.as_deref())
    .bind(raw_api_key_secret.as_deref())
    .bind(api_key_extra_headers_json.as_deref())
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref de) = e
            && de.message().contains("UNIQUE")
        {
            return ClewdrError::Conflict {
                msg: "account name or rr_order already exists",
            };
        }
        ClewdrError::from(e)
    })?
    .last_insert_rowid();

    tx.commit().await?;

    if let Some(ref oauth) = oauth {
        batch_upsert_runtime_states(&db, &[(id, oauth.snapshot.runtime.clone())]).await?;
    }

    let _ = actor.reload_from_db().await;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

pub async fn update(
    State(db): State<SqlitePool>,
    State(actor): State<AccountPoolHandle>,
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
    let requested_proxy_id = req.proxy_id.and_then(|value| (value > 0).then_some(value));
    let proxy_binding = if req.proxy_id.is_some() {
        resolve_proxy_url(&db, requested_proxy_id).await?
    } else {
        match (existing.proxy_id, existing.proxy_url.clone()) {
            (Some(proxy_id), Some(url)) => Some((proxy_id, url)),
            _ => None,
        }
    };
    let new_cookie_blob = normalize_cookie_blob(req.cookie_blob.clone())?;
    let oauth_state = normalize_optional(req.oauth_state.clone());
    let oauth_callback_input = normalize_optional(req.oauth_callback_input.clone());

    // ApiKey inputs: same fast-fail validation as create(). On update,
    // empty / missing fields mean "keep existing" within an api_key→
    // api_key update; switching INTO api_key from cookie/oauth requires
    // both base_url + secret to be present (checked below).
    let raw_new_api_key_base_url = normalize_optional(req.api_key_base_url.clone());
    let raw_new_api_key_secret = normalize_optional(req.api_key_secret.clone());
    let api_key_extra_headers_payload = req.api_key_extra_headers.clone();
    let new_api_key_base_url_normalized: Option<String> = match raw_new_api_key_base_url.as_deref()
    {
        Some(raw) => Some(normalize_api_key_base_url(raw)?.as_str().to_string()),
        None => None,
    };
    if let Some(ref map) = api_key_extra_headers_payload {
        validate_api_key_extra_headers(map)?;
    }
    let submitting_api_key = raw_new_api_key_base_url.is_some()
        || raw_new_api_key_secret.is_some()
        || api_key_extra_headers_payload.is_some();

    let submitted_count = new_cookie_blob.is_some() as u8
        + oauth_callback_input.is_some() as u8
        + submitting_api_key as u8;
    if submitted_count > 1 {
        return Err(ClewdrError::BadRequest {
            msg: "Submit exactly one credential kind: cookie, OAuth callback, or API key",
        });
    }
    let oauth = match oauth_callback_input {
        Some(input) => Some(
            exchange_admin_oauth_callback(
                &input,
                oauth_state.as_deref(),
                proxy_binding.as_ref().map(|(_, url)| url.as_str()),
            )
            .await?,
        ),
        None => None,
    };
    derive_auth_source(
        req.auth_source.as_deref(),
        new_cookie_blob.is_some(),
        oauth.is_some(),
        submitting_api_key,
        Some(existing.auth_source.as_str()),
    )?;

    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(ref oauth) = oauth {
        reject_duplicate_oauth_identity(&mut *tx, &oauth.snapshot.organization_uuid, Some(id))
            .await?;
    }

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
    if req.proxy_id.is_some() {
        sqlx::query(
            "UPDATE accounts SET proxy_id = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
        )
        .bind(requested_proxy_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(drain_first) = req.drain_first {
        sqlx::query(
            "UPDATE accounts SET drain_first = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
        )
        .bind(drain_first as i64)
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
                 last_failure_json = NULL,
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
        // Single-statement credential replacement: cookie_blob, auth_source,
        // and cleared oauth / api_key fields are written together so the
        // row-level credential-mutex CHECK is only evaluated against the
        // final state. Piecewise updates (write cookie first, then clear
        // oauth, then switch auth_source) would trip the CHECK
        // mid-transaction. Step 5 / C10: also NULL the api_key_*
        // columns so switching api_key → cookie clears the prior
        // credential and the mutex CHECK accepts the row.
        sqlx::query(
            "UPDATE accounts
             SET cookie_blob = ?1,
                 oauth_access_token = NULL,
                 oauth_refresh_token = NULL,
                 oauth_expires_at = NULL,
                 last_refresh_at = NULL,
                 organization_uuid = NULL,
                 email = NULL,
                 account_type = NULL,
                 rate_limit_tier = NULL,
                 subscription_created_at = NULL,
                 billing_type = NULL,
                 api_key_base_url = NULL,
                 api_key_secret = NULL,
                 api_key_extra_headers = NULL,
                 auth_source = 'cookie',
                 status = 'active',
                 invalid_reason = NULL,
                 last_error = NULL,
                 last_failure_json = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2",
        )
        .bind(blob)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    } else if let Some(ref oauth_data) = oauth {
        // Mirror of the cookie branch: all credential + auth_source columns
        // move together in a single UPDATE so the mutex CHECK sees only the
        // consistent post-write state. C10: NULL api_key_* for the same
        // reason as the cookie branch above.
        sqlx::query(
            "UPDATE accounts
             SET cookie_blob = NULL,
                 oauth_access_token = ?1,
                 oauth_refresh_token = ?2,
                 oauth_expires_at = ?3,
                 last_refresh_at = ?4,
                 organization_uuid = ?5,
                 api_key_base_url = NULL,
                 api_key_secret = NULL,
                 api_key_extra_headers = NULL,
                 auth_source = 'oauth',
                 status = 'active',
                 last_error = NULL,
                 invalid_reason = NULL,
                 last_failure_json = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?6",
        )
        .bind(oauth_data.token.access_token.as_str())
        .bind(oauth_data.token.refresh_token.as_str())
        .bind(oauth_data.token.expires_at.to_rfc3339())
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(oauth_data.snapshot.organization_uuid.as_str())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    } else if submitting_api_key {
        // ApiKey branch. Two sub-cases:
        //   - Switching INTO api_key from cookie/oauth: both base_url
        //     and secret MUST be present (the schema mutex CHECK would
        //     reject a partial row anyway; surface a clean 400 here).
        //   - Updating WITHIN api_key: any subset of (base_url, secret,
        //     extra_headers) may be present; missing fields keep the
        //     existing value (mirror cookie/oauth empty-means-keep
        //     semantics at L606, L648).
        //
        // extra_headers tri-state on the request side:
        //   - None / omitted → keep existing JSON column unchanged
        //   - Some({}) → explicit clear (NULL the column)
        //   - Some(map) non-empty → replace with serialized JSON
        let switching_into_api_key = existing.auth_source != "api_key";

        let final_base_url: String = match (
            new_api_key_base_url_normalized.as_deref(),
            existing.api_key_base_url.as_deref(),
            switching_into_api_key,
        ) {
            (Some(v), _, _) => v.to_string(),
            (None, Some(existing_url), false) => existing_url.to_string(),
            _ => {
                return Err(ClewdrError::BadRequest {
                    msg: "api_key_base_url is required when switching to api_key",
                });
            }
        };

        let final_secret: String = match (
            raw_new_api_key_secret.as_deref(),
            existing.api_key_secret.as_deref(),
            switching_into_api_key,
        ) {
            (Some(v), _, _) => v.to_string(),
            (None, Some(existing_secret), false) => existing_secret.to_string(),
            _ => {
                return Err(ClewdrError::BadRequest {
                    msg: "api_key_secret is required when switching to api_key",
                });
            }
        };

        let final_extra_headers_sql: Option<String> = match api_key_extra_headers_payload {
            None => existing.api_key_extra_headers.clone(),
            Some(map) => extra_headers_to_db(&map),
        };

        sqlx::query(
            "UPDATE accounts
             SET cookie_blob = NULL,
                 oauth_access_token = NULL,
                 oauth_refresh_token = NULL,
                 oauth_expires_at = NULL,
                 last_refresh_at = NULL,
                 organization_uuid = NULL,
                 email = NULL,
                 account_type = NULL,
                 rate_limit_tier = NULL,
                 subscription_created_at = NULL,
                 billing_type = NULL,
                 api_key_base_url = ?1,
                 api_key_secret = ?2,
                 api_key_extra_headers = ?3,
                 auth_source = 'api_key',
                 status = 'active',
                 invalid_reason = NULL,
                 last_error = NULL,
                 last_failure_json = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?4",
        )
        .bind(final_base_url)
        .bind(final_secret)
        .bind(final_extra_headers_sql)
        .bind(id)
        .execute(&mut *tx)
        .await?;

        // When switching INTO api_key from cookie/oauth, drop the
        // stale subscription runtime row. ApiKey has no quota window /
        // cooldown / count_tokens_allowed gate (PRD Decision 2), so a
        // leftover row from the previous credential life would produce
        // wrong behavior: a stale `reset_time` would park the slot in
        // `exhausted`, a stale `count_tokens_allowed = false` would
        // route count_tokens to the local estimator, and stale weekly
        // utilization buckets would surface as misleading dashboard
        // numbers. The loader carries the same guard as defense in
        // depth (see account_pool.rs cold-restart branch); this is the
        // proactive admin-side cleanup so the DB never carries
        // misleading data for an ApiKey account.
        //
        // Within-api_key updates don't need this (runtime stays empty
        // throughout an ApiKey account's life), so gate on the
        // switch-in flag.
        if switching_into_api_key {
            sqlx::query("DELETE FROM account_runtime_state WHERE account_id = ?1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
    }
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
        update_account_metadata_unchecked(
            &db,
            id,
            crate::db::accounts::AccountMetadataUpdate {
                email: oauth.snapshot.email.as_deref(),
                account_type: oauth.snapshot.account_type.as_deref(),
                organization_uuid: Some(oauth.snapshot.organization_uuid.as_str()),
                rate_limit_tier: oauth.snapshot.rate_limit_tier.as_deref(),
                subscription_created_at: oauth.snapshot.subscription_created_at.as_deref(),
                billing_type: oauth.snapshot.billing_type.as_deref(),
            },
        )
        .await?;
        batch_upsert_runtime_states(&db, &[(id, oauth.snapshot.runtime.clone())]).await?;
    }

    let _ = actor.reload_from_db().await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn remove(
    State(db): State<SqlitePool>,
    State(actor): State<AccountPoolHandle>,
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

/// HTTP `POST /accounts/probe`. Per Step 4 / C4, this delegates the
/// cookie-vs-oauth split to the pool's unified probe entry point. The
/// admin still computes eligibility (OAuth: must be non-disabled with a
/// stored token; cookie: must have a `cookie_blob`) so disabled OAuth
/// accounts are not auto-reactivated by an admin probe — that asymmetry
/// matches pre-C4 behavior. Pool's `spawn_probe_guarded` then DB-loads
/// each row and dispatches to `probe_cookie` or `probe_oauth_account`.
pub async fn probe_all(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ClewdrError> {
    let accounts = load_all_accounts(&state.db).await?;
    let eligible_ids: Vec<i64> = accounts
        .into_iter()
        .filter(|a| match a.auth_source.as_str() {
            "oauth" => a.status != "disabled" && a.oauth_token.is_some(),
            // ApiKey accounts have no subscription quota window and no
            // OAuth profile to refresh; the probe machinery is
            // subscription-shaped and not applicable. Explicit arm
            // makes the intent clear vs leaning on the cookie default.
            "api_key" => false,
            _ => a.cookie_blob.is_some(),
        })
        .map(|a| a.id)
        .collect();

    let probing_ids = if eligible_ids.is_empty() {
        Vec::new()
    } else {
        state
            .account_pool
            .probe_accounts(eligible_ids, state.event_tx.clone())
            .await?
    };

    Ok(Json(serde_json::json!({ "probing_ids": probing_ids })))
}

// ---------------------------------------------------------------------------
// Credential test — minimal /v1/messages probe
// ---------------------------------------------------------------------------

const TEST_ACCOUNT_MODEL: &str = "claude-haiku-4-5-20251001";

#[derive(Serialize)]
pub struct TestAccountResponse {
    pub success: bool,
    pub latency_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

fn test_response_error(status_u16: u16, response_body: &str) -> ClewdrError {
    let code =
        wreq::StatusCode::from_u16(status_u16).unwrap_or(wreq::StatusCode::INTERNAL_SERVER_ERROR);
    claude_error_from_response_parts(code, None, response_body)
}

async fn persist_test_failure_verdict(
    state: &AppState,
    account_id: i64,
    failure: &AccountFailureContext,
    message: &str,
) {
    let Some(reason) = failure.normalized_reason.to_reason() else {
        return;
    };
    match failure.action {
        AccountFailureAction::TerminalDisabled => {
            if let Err(err) =
                set_account_disabled(&state.db, account_id, &reason.to_db_string()).await
            {
                tracing::warn!("Failed to set test-disabled account {account_id}: {err}");
                return;
            }
        }
        AccountFailureAction::TerminalAuth => {
            if let Err(err) = set_account_auth_error(&state.db, account_id, message).await {
                tracing::warn!("Failed to set test auth_error account {account_id}: {err}");
                return;
            }
        }
        AccountFailureAction::Cooldown { reset_time } => {
            if let Err(err) = set_account_reset_time(&state.db, account_id, reset_time).await {
                tracing::warn!("Failed to set test cooldown for account {account_id}: {err}");
                return;
            }
            if let Err(err) = state.account_pool.reload_from_db().await {
                tracing::warn!(
                    "Failed to reload pool after test cooldown for account {account_id}: {err}"
                );
            }
            return;
        }
        AccountFailureAction::TransientUpstream | AccountFailureAction::InternalError => return,
    }

    let persisted = AccountFailureContextPersisted::from(failure);
    if let Err(err) = set_account_last_failure(&state.db, account_id, Some(&persisted)).await {
        tracing::warn!("Failed to persist test failure for account {account_id}: {err}");
    }
    state.account_pool.invalidate(account_id, reason).await;
}

/// Clean up after a successful `/test`: drop any persisted failure verdict
/// and clear the in-memory probe-error marker, then **reactivate** if the
/// account was sitting in `auth_error`.
///
/// The reactivation half mirrors the OAuth probe success tail in
/// `claude_code_state/probe.rs` (search `did_reactivate`). Without it,
/// `auth_source = 'api_key'` accounts have no recovery path — they have
/// no `/probe` endpoint and `/test` only cleared the failure metadata,
/// leaving the row stuck in `auth_error` and the pool's `state.invalid`
/// set. Cookie / OAuth callers were not stuck because `/probe` could
/// reactivate them, but extending the behavior here also fixes the
/// "test succeeded but pool still won't dispatch" asymmetry for them.
///
/// `previous_status` is the status observed when the test handler loaded
/// the account; using the start-of-test snapshot matches the OAuth
/// probe pattern and side-steps a concurrent flip happening mid-test.
async fn clear_test_failure_verdict(state: &AppState, account_id: i64, previous_status: &str) {
    if let Err(err) = set_account_last_failure(&state.db, account_id, None).await {
        tracing::warn!("Failed to clear test failure for account {account_id}: {err}");
    }
    state.account_pool.clear_probe_error(account_id).await;

    if previous_status == "auth_error" {
        match set_account_active(&state.db, account_id).await {
            Ok(()) => {
                // Drop any stale cooldown left over from an earlier 429
                // before we flipped to auth_error. Without this, the
                // pool loader sees a future `reset_time` and immediately
                // re-parks the just-reactivated row in
                // exhausted/cooling — the row would read as `active` in
                // the DB but never dispatch. The OAuth probe path
                // doesn't need this because it writes a fresh
                // `reset_time` from the quota snapshot in the same
                // pass; `/test` has no such snapshot.
                if let Err(err) = clear_account_cooldown(&state.db, account_id).await {
                    tracing::warn!(
                        "Failed to clear cooldown for reactivated account {account_id}: {err}"
                    );
                }
                // `set_account_active` has a `status != 'disabled'` guard,
                // so a concurrent admin disable still wins; reload the
                // pool so its in-memory invalid set picks up whichever
                // outcome landed.
                if let Err(err) = state.account_pool.reload_from_db().await {
                    tracing::warn!(
                        "Failed to reload pool after test reactivation for account {account_id}: {err}"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    "Failed to reactivate account {account_id} after successful test: {err}"
                );
            }
        }
    }
}

/// `/test` flow for `auth_source = 'api_key'` slots. The cookie /
/// OAuth branch carries a long token-refresh ladder that ApiKey does
/// not need; rather than weave conditional logic through 240 lines of
/// existing code, the ApiKey case lives here as a self-contained
/// branch that mirrors the same shape: send → classify → log →
/// persist verdict → return `TestAccountResponse`.
async fn test_account_api_key(
    state: AppState,
    id: i64,
    account: AccountWithRuntime,
    started_at: chrono::DateTime<chrono::Utc>,
) -> Result<Json<TestAccountResponse>, ClewdrError> {
    // Captured before any potential field consumption so we can later
    // pass it into `clear_test_failure_verdict` for the reactivation
    // path (see that helper's doc comment).
    let previous_status = account.status.clone();
    let base_url = account
        .api_key_base_url
        .as_deref()
        .ok_or(ClewdrError::BadRequest {
            msg: "api_key account missing base_url",
        })?;
    let secret = account
        .api_key_secret
        .as_deref()
        .ok_or(ClewdrError::BadRequest {
            msg: "api_key account missing secret",
        })?;
    // Defensive re-normalization: admin write-time validation ran when
    // the row was inserted, but a manual DB edit could skip it.
    let normalized = normalize_api_key_base_url(base_url)?;
    let url = normalized
        .join("v1/messages")
        .expect("normalized base url joins cleanly");
    let mut url_with_query = url;
    url_with_query.set_query(Some("beta=true"));
    let request_url = url_with_query.to_string();

    let extras: BTreeMap<String, String> = account
        .api_key_extra_headers
        .as_deref()
        .and_then(|s| serde_json::from_str::<BTreeMap<String, String>>(s).ok())
        .unwrap_or_default();

    let body = serde_json::json!({
        "model": TEST_ACCOUNT_MODEL,
        "max_tokens": 10,
        "messages": [{"role": "user", "content": "reply with ok only"}],
        "stream": false,
    });

    let client = build_api_client(account.proxy_url.as_deref());
    let mut req = client
        .post(&request_url)
        .header("x-api-key", secret)
        .header("anthropic-version", "2023-06-01");
    for (k, v) in extras.iter() {
        if is_reserved_api_key_extra_header(k) {
            continue;
        }
        req = req.header(k.as_str(), v.as_str());
    }
    let result = req.json(&body).send().await.context(WreqSnafu {
        msg: "test request failed",
    });
    let latency_ms = (chrono::Utc::now() - started_at).num_milliseconds();

    let (success, http_status, error_msg, response_body) = match result {
        Ok(resp) => {
            let status_code = resp.status().as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            if (200..300).contains(&status_code) {
                (true, Some(status_code), None, body_text)
            } else {
                (false, Some(status_code), Some(body_text.clone()), body_text)
            }
        }
        Err(e) => (false, None, Some(e.to_string()), String::new()),
    };

    // Classify with `Some(AuthMethod::ApiKey)` so 429 → TransientUpstream
    // (Cooldown is downgraded at the classifier chokepoint per C9).
    // `persist_test_failure_verdict` then sees TransientUpstream and
    // returns without mutating account state — correct for ApiKey.
    let failure = if success {
        None
    } else if let Some(status_u16) = http_status {
        let synthetic = test_response_error(status_u16, &response_body);
        Some(classify_account_failure(
            &synthetic,
            FailureSource::Test,
            None,
            Some(AuthMethod::ApiKey),
        ))
    } else {
        None
    };
    let log_status = if success {
        "ok"
    } else if let Some(failure) = &failure {
        failure.action.to_log_status()
    } else {
        "upstream_error"
    };
    let ctx = BillingContext {
        db: state.db.clone(),
        user_id: None,
        api_key_id: None,
        account_id: Some(id),
        model_raw: TEST_ACCOUNT_MODEL.to_string(),
        request_id: format!("test-{}-{}", id, uuid::Uuid::new_v4()),
        started_at,
        event_tx: state.event_tx.clone(),
        // Admin-driven infrastructure test, no associated api_key.
        audit: None,
    };
    persist_probe_log(
        &ctx,
        RequestType::Test,
        log_status,
        http_status,
        &response_body,
        error_msg.as_deref(),
    )
    .await;
    if success {
        clear_test_failure_verdict(&state, id, &previous_status).await;
    } else if let (Some(failure), Some(message)) = (&failure, error_msg.as_deref()) {
        persist_test_failure_verdict(&state, id, failure, message).await;
    }

    Ok(Json(TestAccountResponse {
        success,
        latency_ms,
        error: error_msg,
        http_status,
    }))
}

pub async fn test_account(
    State(state): State<AppState>,
    State(profile): State<SharedStealthProfile>,
    Path(id): Path<i64>,
) -> Result<Json<TestAccountResponse>, ClewdrError> {
    // 1. Load account
    let account = get_account_by_id(&state.db, id)
        .await?
        .ok_or(ClewdrError::NotFound {
            msg: "account not found",
        })?;

    if account.status == "disabled" {
        return Err(ClewdrError::BadRequest {
            msg: "account is disabled",
        });
    }

    // Snapshot for the reactivation path in clear_test_failure_verdict.
    let previous_status = account.status.clone();

    let started_at = chrono::Utc::now();

    // ApiKey accounts have no bearer-token ladder — skip the OAuth /
    // cookie token-fetch entirely and send directly with `x-api-key` +
    // extra headers (after the reserved-name filter, defense in depth
    // against the same admin-side validator that already ran at write
    // time). The response-handling tail (classify + log + verdict) is
    // intentionally near-duplicate of the OAuth/cookie path below, with
    // the one difference that the classifier is told this is an ApiKey
    // slot so 429 → TransientUpstream (PRD Decision 2) instead of
    // Cooldown — which means `persist_test_failure_verdict`'s
    // TransientUpstream / InternalError arms are no-ops for ApiKey,
    // which is exactly the desired "pay-as-you-go has no cooldown"
    // behavior.
    if account.auth_source == "api_key" {
        return test_account_api_key(state, id, account, started_at).await;
    }

    let access_token = match account.oauth_token.clone() {
        Some(token) => {
            if token.is_expired() {
                let _guard = crate::services::oauth_refresh_guard::guard().lock(id).await;
                let token = if let Some(t) = state.account_pool.get_token(id).await.unwrap_or(None)
                {
                    t
                } else {
                    match get_account_by_id(&state.db, id).await {
                        Ok(Some(acc)) => acc.oauth_token.unwrap_or(token),
                        _ => token,
                    }
                };
                if !token.is_expired() {
                    token.access_token
                } else {
                    match refresh_oauth_token(&token, account.proxy_url.as_deref()).await {
                        Ok(refreshed) => {
                            let persisted = match upsert_account_oauth(
                                &state.db,
                                id,
                                Some(&refreshed.token),
                                None,
                                Some(&token.refresh_token),
                            )
                            .await
                            {
                                Ok(persisted) => persisted,
                                Err(db_err) => {
                                    let error_msg =
                                        format!("failed to persist refreshed token: {db_err}");
                                    let ctx = BillingContext {
                                        db: state.db.clone(),
                                        user_id: None,
                                        api_key_id: None,
                                        account_id: Some(id),
                                        model_raw: TEST_ACCOUNT_MODEL.to_string(),
                                        request_id: format!("test-{}-{}", id, uuid::Uuid::new_v4()),
                                        started_at,
                                        event_tx: state.event_tx.clone(),
                                        audit: None,
                                    };
                                    persist_probe_log(
                                        &ctx,
                                        RequestType::Test,
                                        "internal_error",
                                        None,
                                        "",
                                        Some(&error_msg),
                                    )
                                    .await;
                                    return Ok(Json(TestAccountResponse {
                                        success: false,
                                        latency_ms: (chrono::Utc::now() - started_at)
                                            .num_milliseconds(),
                                        error: Some(error_msg),
                                        http_status: None,
                                    }));
                                }
                            };
                            if !persisted {
                                match get_account_by_id(&state.db, id).await {
                                    Ok(Some(account)) => match account.oauth_token {
                                        Some(current) => current.access_token,
                                        None => {
                                            return Err(ClewdrError::InvalidAuth);
                                        }
                                    },
                                    _ => return Err(ClewdrError::InvalidAuth),
                                }
                            } else {
                                let updated = state
                                    .account_pool
                                    .update_credential_if_current(
                                        id,
                                        &token.refresh_token,
                                        Some(refreshed.token.clone()),
                                    )
                                    .await?;
                                if updated {
                                    refreshed.token.access_token
                                } else {
                                    match get_account_by_id(&state.db, id).await {
                                        Ok(Some(account)) => match account.oauth_token {
                                            Some(current) => current.access_token,
                                            None => return Err(ClewdrError::InvalidAuth),
                                        },
                                        _ => return Err(ClewdrError::InvalidAuth),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let error_msg = e.to_string();
                            // Step 3.5 C3a: derive log status from the
                            // classifier so an OAuth refresh that hits a
                            // local error (e.g. transport / serde) is no
                            // longer hardcoded to `auth_rejected` — it
                            // surfaces as `internal_error` while real
                            // refresh-token rejections (`invalid_grant`,
                            // 401/403) keep the auth_rejected verdict.
                            let failure = classify_account_failure(
                                &e,
                                FailureSource::Test,
                                Some("refresh"),
                                None,
                            );
                            let log_status = failure.action.to_log_status();
                            let ctx = BillingContext {
                                db: state.db.clone(),
                                user_id: None,
                                api_key_id: None,
                                account_id: Some(id),
                                model_raw: TEST_ACCOUNT_MODEL.to_string(),
                                request_id: format!("test-{}-{}", id, uuid::Uuid::new_v4()),
                                started_at,
                                event_tx: state.event_tx.clone(),
                                audit: None,
                            };
                            persist_probe_log(
                                &ctx,
                                RequestType::Test,
                                log_status,
                                None,
                                "",
                                Some(&error_msg),
                            )
                            .await;
                            persist_test_failure_verdict(&state, id, &failure, &error_msg).await;
                            return Ok(Json(TestAccountResponse {
                                success: false,
                                latency_ms: (chrono::Utc::now() - started_at).num_milliseconds(),
                                error: Some(error_msg),
                                http_status: None,
                            }));
                        }
                    }
                }
            } else {
                token.access_token
            }
        }
        None => {
            let cookie_blob = account
                .cookie_blob
                .as_deref()
                .ok_or(ClewdrError::BadRequest {
                    msg: "account has no usable credential",
                })?;
            let mut slot = AccountSlot::new(cookie_blob, None)?;
            slot.account_id = Some(id);
            slot.proxy_url = account.proxy_url.clone();
            let mut cc_state = ClaudeCodeState::from_credential(
                state.account_pool.clone(),
                slot,
                profile.clone(),
            )?;
            match cc_state.check_token() {
                crate::claude_code_state::TokenStatus::None => {
                    let org = cc_state.get_organization().await?;
                    let code = cc_state.exchange_code(&org).await?;
                    cc_state.exchange_token(code).await?;
                }
                crate::claude_code_state::TokenStatus::Expired => {
                    cc_state.refresh_token().await?;
                }
                crate::claude_code_state::TokenStatus::Valid => {}
            }
            let token = cc_state
                .cookie
                .as_ref()
                .and_then(|slot| slot.token.as_ref())
                .cloned()
                .ok_or(ClewdrError::UnexpectedNone {
                    msg: "No access token found after cookie credential exchange",
                })?;
            state
                .account_pool
                .update_credential(id, Some(token.clone()))
                .await;
            token.access_token
        }
    };

    let body = serde_json::json!({
        "model": TEST_ACCOUNT_MODEL,
        "max_tokens": 10,
        "messages": [{"role": "user", "content": "reply with ok only"}],
        "stream": false,
    });

    // 5. Send request
    let client = build_api_client(account.proxy_url.as_deref());
    let url = format!("{CLAUDE_ENDPOINT}v1/messages?beta=true");
    let ua = profile.load().user_agent();

    let result = client
        .post(&url)
        .bearer_auth(&access_token)
        .header(USER_AGENT, ua)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context(WreqSnafu {
            msg: "test request failed",
        });
    let latency_ms = (chrono::Utc::now() - started_at).num_milliseconds();

    // 6. Process response
    let (success, http_status, error_msg, response_body) = match result {
        Ok(resp) => {
            let status_code = resp.status().as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            if (200..300).contains(&status_code) {
                (true, Some(status_code), None, body_text)
            } else {
                (false, Some(status_code), Some(body_text.clone()), body_text)
            }
        }
        Err(e) => (false, None, Some(e.to_string()), String::new()),
    };

    // 7. Log result
    // Step 3.5 C3b: derive log status from the same upstream-error parser
    // and classifier used by /v1/messages, so /test preserves body phrases
    // like "organization has been disabled" instead of reducing them to a
    // bare HTTP status.
    let failure = if success {
        None
    } else if let Some(status_u16) = http_status {
        let synthetic = test_response_error(status_u16, &response_body);
        Some(classify_account_failure(
            &synthetic,
            FailureSource::Test,
            None,
            None,
        ))
    } else {
        None
    };
    let log_status = if success {
        "ok"
    } else if let Some(failure) = &failure {
        failure.action.to_log_status()
    } else {
        "upstream_error"
    };
    let ctx = BillingContext {
        db: state.db.clone(),
        user_id: None,
        api_key_id: None,
        account_id: Some(id),
        model_raw: TEST_ACCOUNT_MODEL.to_string(),
        request_id: format!("test-{}-{}", id, uuid::Uuid::new_v4()),
        started_at,
        event_tx: state.event_tx.clone(),
        // Admin-driven infrastructure test, no associated api_key.
        audit: None,
    };
    persist_probe_log(
        &ctx,
        RequestType::Test,
        log_status,
        http_status,
        &response_body,
        error_msg.as_deref(),
    )
    .await;
    if success {
        clear_test_failure_verdict(&state, id, &previous_status).await;
    } else if let (Some(failure), Some(message)) = (&failure, error_msg.as_deref()) {
        persist_test_failure_verdict(&state, id, failure, message).await;
    }

    Ok(Json(TestAccountResponse {
        success,
        latency_ms,
        error: error_msg,
        http_status,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Reason;

    #[test]
    fn derives_cookie_when_only_cookie_submitted() {
        assert_eq!(
            derive_auth_source(None, true, false, false, None).unwrap(),
            "cookie"
        );
        assert_eq!(
            derive_auth_source(None, true, false, false, Some("oauth")).unwrap(),
            "cookie"
        );
    }

    #[test]
    fn derives_oauth_when_only_oauth_submitted() {
        assert_eq!(
            derive_auth_source(None, false, true, false, None).unwrap(),
            "oauth"
        );
        assert_eq!(
            derive_auth_source(None, false, true, false, Some("cookie")).unwrap(),
            "oauth"
        );
    }

    #[test]
    fn derives_api_key_when_only_api_key_submitted() {
        assert_eq!(
            derive_auth_source(None, false, false, true, None).unwrap(),
            "api_key"
        );
        // Switching from cookie/oauth → api_key: the submitted credential wins.
        assert_eq!(
            derive_auth_source(None, false, false, true, Some("cookie")).unwrap(),
            "api_key"
        );
        assert_eq!(
            derive_auth_source(None, false, false, true, Some("oauth")).unwrap(),
            "api_key"
        );
    }

    #[test]
    fn preserves_existing_when_nothing_submitted() {
        assert_eq!(
            derive_auth_source(None, false, false, false, Some("cookie")).unwrap(),
            "cookie"
        );
        assert_eq!(
            derive_auth_source(None, false, false, false, Some("oauth")).unwrap(),
            "oauth"
        );
        assert_eq!(
            derive_auth_source(None, false, false, false, Some("api_key")).unwrap(),
            "api_key"
        );
    }

    #[test]
    fn errors_when_nothing_submitted_and_no_existing() {
        let err = derive_auth_source(None, false, false, false, None).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
    }

    #[test]
    fn errors_when_existing_is_legacy_hybrid_without_new_credentials() {
        // Post-C3 migration no hybrid rows should remain, but the derivation
        // is defensive: if one slips through, updating without new credentials
        // must fail rather than silently preserve an invalid value.
        let err = derive_auth_source(None, false, false, false, Some("hybrid")).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
    }

    #[test]
    fn accepts_requested_that_matches_derived() {
        assert_eq!(
            derive_auth_source(Some("cookie"), true, false, false, None).unwrap(),
            "cookie"
        );
        assert_eq!(
            derive_auth_source(Some("oauth"), false, true, false, None).unwrap(),
            "oauth"
        );
        assert_eq!(
            derive_auth_source(Some("api_key"), false, false, true, None).unwrap(),
            "api_key"
        );
    }

    #[test]
    fn errors_on_requested_mismatch() {
        let err = derive_auth_source(Some("oauth"), true, false, false, None).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
        // Requesting api_key while submitting cookie is also a mismatch.
        let err = derive_auth_source(Some("api_key"), true, false, false, None).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
    }

    #[test]
    fn rejects_legacy_hybrid_request() {
        // Requesting auth_source="hybrid" with a single valid credential must
        // fail at the requested-vs-derived mismatch check.
        let err = derive_auth_source(Some("hybrid"), true, false, false, None).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
    }

    #[test]
    fn rejects_dual_credential_submission() {
        let err = derive_auth_source(None, true, true, false, None).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
        // cookie + api_key
        let err = derive_auth_source(None, true, false, true, None).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
        // oauth + api_key
        let err = derive_auth_source(None, false, true, true, None).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
        // All three.
        let err = derive_auth_source(None, true, true, true, None).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
    }

    #[test]
    fn validate_api_key_extra_headers_accepts_safe_keys() {
        let mut map = BTreeMap::new();
        map.insert("anthropic-workspace-id".to_string(), "ws-123".to_string());
        map.insert("x-custom-header".to_string(), "value".to_string());
        assert!(validate_api_key_extra_headers(&map).is_ok());
        // Empty map is fine (caller treats it as "no extras").
        let empty = BTreeMap::new();
        assert!(validate_api_key_extra_headers(&empty).is_ok());
    }

    #[test]
    fn validate_api_key_extra_headers_rejects_reserved_keys() {
        // Case-insensitive: the send-side filter is the source of truth.
        for reserved in [
            "x-api-key",
            "X-API-KEY",
            "Authorization",
            "anthropic-version",
            "anthropic-beta",
            "user-agent",
            "USER-AGENT",
            "host",
            "content-length",
            "content-type",
            "accept-encoding",
        ] {
            let mut map = BTreeMap::new();
            map.insert(reserved.to_string(), "v".to_string());
            let err = validate_api_key_extra_headers(&map).unwrap_err();
            assert!(matches!(
                err,
                ClewdrError::BadRequestMessage { .. } | ClewdrError::BadRequest { .. }
            ));
        }
    }

    #[test]
    fn validate_api_key_extra_headers_rejects_empty_key() {
        let mut map = BTreeMap::new();
        map.insert("".to_string(), "v".to_string());
        let err = validate_api_key_extra_headers(&map).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));

        let mut map = BTreeMap::new();
        map.insert("   ".to_string(), "v".to_string());
        let err = validate_api_key_extra_headers(&map).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
    }

    #[test]
    fn extra_headers_to_db_returns_none_for_empty() {
        assert_eq!(extra_headers_to_db(&BTreeMap::new()), None);
    }

    #[test]
    fn extra_headers_to_db_serializes_btree_map_stably() {
        let mut map = BTreeMap::new();
        map.insert("z".to_string(), "1".to_string());
        map.insert("a".to_string(), "2".to_string());
        let serialized = extra_headers_to_db(&map).expect("non-empty map serializes");
        // BTreeMap iteration is key-sorted, so the JSON output is stable.
        assert_eq!(serialized, r#"{"a":"2","z":"1"}"#);
    }

    #[test]
    fn test_response_error_preserves_org_disabled_body_for_classifier() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"This organization has been disabled."}}"#;
        let err = test_response_error(400, body);
        let ctx = classify_account_failure(&err, FailureSource::Test, None, None);
        assert_eq!(ctx.action, AccountFailureAction::TerminalDisabled);
        assert_eq!(ctx.normalized_reason.to_reason(), Some(Reason::Disabled));
    }

    #[test]
    fn test_response_error_preserves_rate_limit_reset_for_classifier() {
        let reset_time = chrono::Utc::now().timestamp() + 3600;
        let body = format!(
            r#"{{"type":"error","error":{{"type":"rate_limit_error","message":"{{\"resetsAt\":{reset_time}}}"}}}}"#
        );
        let err = test_response_error(429, &body);
        let ctx = classify_account_failure(&err, FailureSource::Test, None, None);
        assert_eq!(ctx.action, AccountFailureAction::Cooldown { reset_time });
        assert_eq!(
            ctx.normalized_reason.to_reason(),
            Some(Reason::TooManyRequest(reset_time))
        );
    }
}
