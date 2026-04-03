use sqlx::SqlitePool;
use tracing::{info, warn};

use super::ClaudeCodeState;
use crate::{
    config::{CookieStatus, Reason},
    db::accounts::update_account_metadata,
    error::ClewdrError,
    services::cookie_actor::CookieActorHandle,
    stealth::SharedStealthProfile,
};

/// Propagate InvalidCookie errors, treat everything else as transient.
fn extract_cookie_reason(e: &ClewdrError) -> Option<Reason> {
    if let ClewdrError::InvalidCookie { reason } = e {
        Some(reason.clone())
    } else {
        None
    }
}

/// Probes a single cookie: bootstrap (email/tier/org) + OAuth + usage boundaries.
/// Runs in a spawned task, does not block the actor.
pub async fn probe_cookie(
    account_id: i64,
    cookie: CookieStatus,
    handle: CookieActorHandle,
    profile: SharedStealthProfile,
    db: SqlitePool,
) {
    let cookie_ellipse = cookie.cookie.ellipse();
    let cookie_prefix = &cookie.cookie[..20.min(cookie.cookie.len())];
    let cookie_prefix = cookie_prefix.to_string();
    info!("[probe] starting for account {account_id} ({cookie_ellipse})");

    let mut state = match ClaudeCodeState::from_cookie(handle.clone(), cookie, profile) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("init failed: {e}");
            warn!("[probe] account {account_id}: {msg}");
            handle.set_probe_error(account_id, msg).await;
            let _ = handle.clear_probing(account_id).await;
            return;
        }
    };

    // 1. Bootstrap probe
    let info = match state.fetch_bootstrap_info().await {
        Ok(info) => info,
        Err(e) => {
            if let Some(reason) = extract_cookie_reason(&e) {
                warn!("[probe] account {account_id} invalid: {reason}");
                state.return_cookie(Some(reason)).await;
            } else {
                let msg = format!("bootstrap failed: {e}");
                warn!("[probe] account {account_id} (transient): {msg}");
                handle.set_probe_error(account_id, msg).await;
                let _ = handle.clear_probing(account_id).await;
            }
            return;
        }
    };

    info!(
        "[probe] account {account_id}: email={}, type={}, org={}",
        info.email, info.account_type, info.org_uuid
    );

    // 2. Persist metadata to DB (with cookie prefix check to prevent stale writes)
    if let Err(e) = update_account_metadata(
        &db,
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
        state.return_cookie(Some(Reason::Free)).await;
        return;
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
                state.return_cookie(Some(reason)).await;
            } else {
                let msg = format!("OAuth code exchange failed: {e}");
                warn!("[probe] account {account_id}: {msg}");
                handle.set_probe_error(account_id, msg).await;
                state.return_cookie(None).await;
            }
            return;
        }
    };
    if let Err(e) = state.exchange_token(code_res).await {
        if let Some(reason) = extract_cookie_reason(&e) {
            state.return_cookie(Some(reason)).await;
        } else {
            let msg = format!("OAuth token exchange failed: {e}");
            warn!("[probe] account {account_id}: {msg}");
            handle.set_probe_error(account_id, msg).await;
            state.return_cookie(None).await;
        }
        return;
    }

    // 5. Fetch usage metrics → set resets_at + has_reset flags
    match state.fetch_usage_metrics().await {
        Ok(usage) => {
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
                state.return_cookie(Some(reason)).await;
                return;
            }
            let msg = format!("usage fetch failed: {e}");
            warn!("[probe] account {account_id}: {msg}");
            handle.set_probe_error(account_id, msg).await;
        }
    }

    // 6. Return updated cookie to actor
    state.return_cookie(None).await;
    info!("[probe] completed for account {account_id}");
}
