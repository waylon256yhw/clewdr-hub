use chrono::Utc;
use serde_json::{Map, Value};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use super::ClaudeCodeState;
use crate::{
    billing::{BillingContext, RequestType, persist_probe_log},
    config::{AccountSlot, Reason},
    db::accounts::{
        AccountWithRuntime, batch_upsert_runtime_states, set_account_active,
        set_account_auth_error, update_account_metadata, update_account_metadata_unchecked,
        upsert_account_oauth,
    },
    error::ClewdrError,
    oauth::refresh_oauth_token_with_raw,
    services::account_pool::AccountPoolHandle,
    state::AdminEvent,
    stealth::SharedStealthProfile,
};

const PROBE_BODY_MAX_BYTES: usize = 262_144;

/// Propagate InvalidCookie errors, treat everything else as transient.
fn extract_cookie_reason(e: &ClewdrError) -> Option<Reason> {
    if let ClewdrError::InvalidCookie { reason } = e {
        Some(reason.clone())
    } else {
        None
    }
}

fn is_oauth_auth_failure(err: &ClewdrError) -> bool {
    match err {
        ClewdrError::ClaudeHttpError { code, .. } => matches!(code.as_u16(), 401 | 403),
        ClewdrError::Whatever { message, .. } => {
            let msg = message.to_ascii_lowercase();
            msg.contains("invalid_grant")
                || msg.contains("refresh token not found")
                || msg.contains("refresh token")
                    && (msg.contains("invalid") || msg.contains("expired"))
                || msg.contains("status 401")
                || msg.contains("status 403")
                || msg.contains("access token")
                    && (msg.contains("expired") || msg.contains("invalid"))
        }
        _ => false,
    }
}

struct ProbeFailure {
    stage: &'static str,
    message: String,
    http_status: Option<u16>,
    is_auth: bool,
}

impl ProbeFailure {
    fn from_err(stage: &'static str, err: &ClewdrError) -> Self {
        let (http_status, is_auth) = match err {
            ClewdrError::ClaudeHttpError { code, .. } => {
                let code = code.as_u16();
                (Some(code), matches!(code, 401 | 403))
            }
            // Cookie-level rejection (free/disabled/rate-limited/null body) carries no HTTP
            // status but is logically an auth-style outcome — surface it that way so the
            // logs UI can filter on `auth_rejected`.
            ClewdrError::InvalidCookie { .. } => (None, true),
            _ => (None, is_oauth_auth_failure(err)),
        };
        Self {
            stage,
            message: err.to_string(),
            http_status,
            is_auth,
        }
    }

    fn to_bundle_entry(&self) -> Value {
        serde_json::json!({
            "stage": self.stage,
            "message": self.message,
            "http_status": self.http_status,
        })
    }
}

async fn persist_probe_row(
    db: &SqlitePool,
    event_tx: broadcast::Sender<AdminEvent>,
    account_id: i64,
    started_at: chrono::DateTime<Utc>,
    request_type: RequestType,
    bundle: &Map<String, Value>,
    outcome: Result<(), &ProbeFailure>,
) {
    let (status, http_status, err_msg): (&str, Option<u16>, Option<String>) = match outcome {
        Ok(()) => ("ok", Some(200), None),
        Err(failure) => {
            let status = if failure.is_auth {
                "auth_rejected"
            } else {
                "upstream_error"
            };
            (status, failure.http_status, Some(failure.message.clone()))
        }
    };
    let mut body = serde_json::to_string(bundle).unwrap_or_else(|_| "{}".to_string());
    if body.len() > PROBE_BODY_MAX_BYTES {
        body = format!(r#"{{"truncated":true,"bytes":{}}}"#, body.len());
    }
    let ctx = BillingContext {
        db: db.clone(),
        user_id: None,
        api_key_id: None,
        account_id: Some(account_id),
        model_raw: String::new(),
        request_id: format!("probe-{}-{}", account_id, uuid::Uuid::new_v4()),
        started_at,
        event_tx,
    };
    persist_probe_log(
        &ctx,
        request_type,
        status,
        http_status,
        &body,
        err_msg.as_deref(),
    )
    .await;
}

/// Probes a single cookie: bootstrap (email/tier/org) + OAuth + usage boundaries.
/// Runs in a spawned task, does not block the actor.
///
/// When `log_sink` is `Some`, the probe result (including any raw upstream JSON
/// bodies that were successfully fetched) is written as a `probe_cookie` row in
/// `request_logs`. Pass `None` for auto-triggered probes to keep the log clean.
pub async fn probe_cookie(
    account_id: i64,
    cookie: AccountSlot,
    handle: AccountPoolHandle,
    profile: SharedStealthProfile,
    db: SqlitePool,
    log_sink: Option<broadcast::Sender<AdminEvent>>,
) {
    let started_at = Utc::now();
    let mut bundle: Map<String, Value> = Map::new();

    let outcome: Result<(), ProbeFailure> = run_cookie_probe(
        account_id,
        cookie,
        handle.clone(),
        profile,
        &db,
        &mut bundle,
    )
    .await;

    if let Err(ref failure) = outcome {
        bundle.insert("error".into(), failure.to_bundle_entry());
    }

    if let Some(tx) = log_sink {
        persist_probe_row(
            &db,
            tx,
            account_id,
            started_at,
            RequestType::ProbeCookie,
            &bundle,
            outcome.as_ref().map(|_| ()),
        )
        .await;
    }
}

async fn run_cookie_probe(
    account_id: i64,
    cookie: AccountSlot,
    handle: AccountPoolHandle,
    profile: SharedStealthProfile,
    db: &SqlitePool,
    bundle: &mut Map<String, Value>,
) -> Result<(), ProbeFailure> {
    let cookie_ellipse = cookie.cookie.ellipse();
    let cookie_prefix = &cookie.cookie[..20.min(cookie.cookie.len())];
    let cookie_prefix = cookie_prefix.to_string();
    info!("[probe] starting for account {account_id} ({cookie_ellipse})");

    let mut state = match ClaudeCodeState::from_cookie(handle.clone(), cookie, profile) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("init failed: {e}");
            warn!("[probe] account {account_id}: {msg}");
            handle.set_probe_error(account_id, msg.clone()).await;
            let _ = handle.clear_probing(account_id).await;
            return Err(ProbeFailure {
                stage: "init",
                message: msg,
                http_status: None,
                is_auth: false,
            });
        }
    };

    // 1. Bootstrap probe
    let info = match state.fetch_bootstrap_info_raw().await {
        Ok((info, raw)) => {
            bundle.insert("bootstrap".into(), raw);
            info
        }
        Err(e) => {
            if let Some(reason) = extract_cookie_reason(&e) {
                warn!("[probe] account {account_id} invalid: {reason}");
                state.release_account(Some(reason)).await;
            } else {
                let msg = format!("bootstrap failed: {e}");
                warn!("[probe] account {account_id} (transient): {msg}");
                handle.set_probe_error(account_id, msg).await;
                let _ = handle.clear_probing(account_id).await;
            }
            return Err(ProbeFailure::from_err("bootstrap", &e));
        }
    };

    info!(
        "[probe] account {account_id}: email={}, type={}, org={}",
        info.email, info.account_type, info.org_uuid
    );

    // 2. Persist metadata to DB (with cookie prefix check to prevent stale writes)
    if let Err(e) = update_account_metadata(
        db,
        account_id,
        &info.email,
        &info.account_type,
        &info.org_uuid,
        &cookie_prefix,
    )
    .await
    {
        warn!("[probe] failed to persist metadata for account {account_id}: {e}");
    }

    // 3. Free account → invalidate
    if info.account_type == "free" {
        state.release_account(Some(Reason::Free)).await;
        return Err(ProbeFailure {
            stage: "bootstrap",
            message: "cookie belongs to a free-tier account and was rejected".to_string(),
            http_status: None,
            is_auth: true,
        });
    }

    // Update in-memory cookie with metadata
    if let Some(ref mut cs) = state.cookie {
        cs.email = Some(info.email);
        cs.account_type = Some(info.account_type);
    }

    // 4. OAuth exchange (pro+ only)
    let code_res = match state.exchange_code(&info.org_uuid).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(reason) = extract_cookie_reason(&e) {
                state.release_account(Some(reason)).await;
            } else {
                let msg = format!("OAuth code exchange failed: {e}");
                warn!("[probe] account {account_id}: {msg}");
                handle.set_probe_error(account_id, msg).await;
                state.release_account(None).await;
            }
            return Err(ProbeFailure::from_err("oauth_code", &e));
        }
    };
    if let Err(e) = state.exchange_token(code_res).await {
        if let Some(reason) = extract_cookie_reason(&e) {
            state.release_account(Some(reason)).await;
        } else {
            let msg = format!("OAuth token exchange failed: {e}");
            warn!("[probe] account {account_id}: {msg}");
            handle.set_probe_error(account_id, msg).await;
            state.release_account(None).await;
        }
        return Err(ProbeFailure::from_err("oauth_token", &e));
    }

    // 5. Fetch usage metrics → set resets_at + has_reset flags
    match state.fetch_usage_metrics().await {
        Ok(usage) => {
            bundle.insert("usage".into(), usage.clone());
            if let Some(ref mut cs) = state.cookie {
                let parse_window = |key: &str| -> (Option<i64>, Option<f64>) {
                    let obj = usage.get(key);
                    let resets_at = obj
                        .and_then(|o| o.get("resets_at"))
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.timestamp());
                    let utilization = obj
                        .and_then(|o| o.get("utilization"))
                        .and_then(|v| v.as_f64());
                    (resets_at, utilization)
                };

                let (session_ts, session_util) = parse_window("five_hour");
                let (weekly_ts, weekly_util) = parse_window("seven_day");
                let (opus_ts, opus_util) = parse_window("seven_day_opus");
                let (sonnet_ts, sonnet_util) = parse_window("seven_day_sonnet");

                cs.session_has_reset = Some(session_ts.is_some());
                cs.weekly_has_reset = Some(weekly_ts.is_some());
                cs.weekly_opus_has_reset = Some(opus_ts.is_some());
                cs.weekly_sonnet_has_reset = Some(sonnet_ts.is_some());

                cs.session_resets_at = session_ts;
                cs.weekly_resets_at = weekly_ts;
                cs.weekly_opus_resets_at = opus_ts;
                cs.weekly_sonnet_resets_at = sonnet_ts;

                cs.session_utilization = session_util;
                cs.weekly_utilization = weekly_util;
                cs.weekly_opus_utilization = opus_util;
                cs.weekly_sonnet_utilization = sonnet_util;

                cs.resets_last_checked_at = Some(chrono::Utc::now().timestamp());

                info!(
                    "[probe] account {account_id} usage: session={:?}% weekly={:?}% opus={:?}% sonnet={:?}%",
                    session_util, weekly_util, opus_util, sonnet_util
                );

                // If session or weekly total hits 100%, set reset_time for cooldown
                // (model-specific windows like opus/sonnet are NOT checked here
                //  to avoid blocking the entire account when only one model is exhausted)
                let cooldown_until = [(session_util, session_ts), (weekly_util, weekly_ts)]
                    .into_iter()
                    .filter(|(util, ts)| util >= &Some(100.0) && ts.is_some())
                    .map(|(_, ts)| ts.unwrap())
                    .max();

                if let Some(ts) = cooldown_until {
                    cs.reset_time = Some(ts);
                    info!("[probe] account {account_id} exhausted, cooldown until {ts}");
                } else {
                    cs.reset_time = None;
                }
            }
            handle.clear_probe_error(account_id).await;
        }
        Err(e) => {
            if let Some(reason) = extract_cookie_reason(&e) {
                state.release_account(Some(reason)).await;
                return Err(ProbeFailure::from_err("usage", &e));
            }
            let msg = format!("usage fetch failed: {e}");
            warn!("[probe] account {account_id}: {msg}");
            handle.set_probe_error(account_id, msg).await;
            // Usage fetch is non-fatal: we still return the cookie so it can serve traffic,
            // but surface the failure to the probe log.
            state.release_account(None).await;
            info!("[probe] completed for account {account_id} (usage fetch failed)");
            return Err(ProbeFailure::from_err("usage", &e));
        }
    }

    // 6. Return updated cookie to actor
    state.release_account(None).await;
    info!("[probe] completed for account {account_id}");
    Ok(())
}

pub async fn probe_oauth_account(
    account: AccountWithRuntime,
    handle: AccountPoolHandle,
    db: SqlitePool,
    log_sink: Option<broadcast::Sender<AdminEvent>>,
) {
    let account_id = account.id;
    let started_at = Utc::now();
    let mut bundle: Map<String, Value> = Map::new();

    let outcome = run_oauth_probe(account, handle, &db, &mut bundle).await;

    if let Err(ref failure) = outcome {
        bundle.insert("error".into(), failure.to_bundle_entry());
    }

    if let Some(tx) = log_sink {
        persist_probe_row(
            &db,
            tx,
            account_id,
            started_at,
            RequestType::ProbeOauth,
            &bundle,
            outcome.as_ref().map(|_| ()),
        )
        .await;
    }
}

async fn run_oauth_probe(
    account: AccountWithRuntime,
    handle: AccountPoolHandle,
    db: &SqlitePool,
    bundle: &mut Map<String, Value>,
) -> Result<(), ProbeFailure> {
    let account_id = account.id;
    info!("[probe][oauth] starting for account {account_id}");

    let Some(token) = account.oauth_token.clone() else {
        let msg = "missing stored OAuth token".to_string();
        warn!("[probe][oauth] account {account_id}: {msg}");
        handle.set_probe_error(account_id, msg.clone()).await;
        let _ = handle.clear_probing(account_id).await;
        return Err(ProbeFailure {
            stage: "token",
            message: msg,
            http_status: None,
            is_auth: false,
        });
    };

    match refresh_oauth_token_with_raw(&token, account.proxy_url.as_deref()).await {
        Ok((refreshed, profile_raw, usage_raw)) => {
            bundle.insert("profile".into(), profile_raw);
            bundle.insert("usage".into(), usage_raw);

            if let Err(err) =
                upsert_account_oauth(db, account_id, Some(&refreshed.token), None).await
            {
                let msg = format!("failed to persist refreshed token: {err}");
                warn!("[probe][oauth] account {account_id}: {msg}");
                handle
                    .set_probe_error(account_id, format!("OAuth probe failed: {msg}"))
                    .await;
                let _ = handle.clear_probing(account_id).await;
                return Err(ProbeFailure {
                    stage: "persist_token",
                    message: msg,
                    http_status: None,
                    is_auth: false,
                });
            }
            if let Err(err) = update_account_metadata_unchecked(
                db,
                account_id,
                refreshed.snapshot.email.as_deref(),
                refreshed.snapshot.account_type.as_deref(),
                Some(refreshed.snapshot.organization_uuid.as_str()),
            )
            .await
            {
                let msg = format!("failed to persist metadata: {err}");
                warn!("[probe][oauth] account {account_id}: {msg}");
                handle
                    .set_probe_error(account_id, format!("OAuth probe failed: {msg}"))
                    .await;
                let _ = handle.clear_probing(account_id).await;
                return Err(ProbeFailure {
                    stage: "persist_metadata",
                    message: msg,
                    http_status: None,
                    is_auth: false,
                });
            }
            if let Err(err) =
                batch_upsert_runtime_states(db, &[(account_id, refreshed.snapshot.runtime.clone())])
                    .await
            {
                let msg = format!("failed to persist runtime: {err}");
                warn!("[probe][oauth] account {account_id}: {msg}");
                handle
                    .set_probe_error(account_id, format!("OAuth probe failed: {msg}"))
                    .await;
                let _ = handle.clear_probing(account_id).await;
                return Err(ProbeFailure {
                    stage: "persist_runtime",
                    message: msg,
                    http_status: None,
                    is_auth: false,
                });
            }
            if account.status == "auth_error"
                && let Err(err) = set_account_active(db, account_id).await
            {
                let msg = format!("failed to reactivate account: {err}");
                warn!("[probe][oauth] account {account_id}: {msg}");
                handle
                    .set_probe_error(account_id, format!("OAuth probe failed: {msg}"))
                    .await;
                let _ = handle.clear_probing(account_id).await;
                return Err(ProbeFailure {
                    stage: "reactivate",
                    message: msg,
                    http_status: None,
                    is_auth: false,
                });
            }
            handle.clear_probe_error(account_id).await;
            let _ = handle.clear_probing(account_id).await;
            info!("[probe][oauth] completed for account {account_id}");
            Ok(())
        }
        Err(err) => {
            let msg = err.to_string();
            warn!("[probe][oauth] account {account_id}: {msg}");
            if is_oauth_auth_failure(&err) {
                if let Err(db_err) = set_account_auth_error(db, account_id, &msg).await {
                    warn!(
                        "[probe][oauth] failed to set auth_error for account {account_id}: {db_err}"
                    );
                }
                handle.clear_probe_error(account_id).await;
            } else {
                handle
                    .set_probe_error(account_id, format!("OAuth probe failed: {msg}"))
                    .await;
            }
            let _ = handle.clear_probing(account_id).await;
            let auth = is_oauth_auth_failure(&err);
            let http_status = if let ClewdrError::ClaudeHttpError { code, .. } = &err {
                Some(code.as_u16())
            } else {
                None
            };
            Err(ProbeFailure {
                stage: "refresh",
                message: msg,
                http_status,
                is_auth: auth,
            })
        }
    }
}
