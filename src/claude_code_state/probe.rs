use chrono::Utc;
use serde_json::{Map, Value};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use super::ClaudeCodeState;
use crate::{
    billing::{BillingContext, RequestType, persist_probe_log},
    config::{AccountSlot, CLEWDR_CONFIG, Reason},
    db::accounts::{
        AccountWithRuntime, account_credential_matches_prefix, batch_upsert_runtime_states,
        get_account_by_id, set_account_active, set_account_auth_error, update_account_metadata,
        upsert_account_oauth,
    },
    error::ClewdrError,
    oauth::{fetch_oauth_snapshot_raw, refresh_oauth_token_with_raw},
    services::account_pool::AccountPoolHandle,
    state::AdminEvent,
    stealth::SharedStealthProfile,
    utils::print_out_text,
};

const PROBE_BODY_MAX_BYTES: usize = 262_144;

fn probe_bundle_component_sizes(bundle: &Map<String, Value>) -> Map<String, Value> {
    let mut sizes = Map::new();
    for (key, value) in bundle {
        let bytes = serde_json::to_vec(value)
            .map(|buf| buf.len())
            .unwrap_or_default();
        sizes.insert(key.clone(), Value::from(bytes as u64));
    }
    sizes
}

fn dump_probe_bundle(
    request_type: RequestType,
    account_id: i64,
    started_at: chrono::DateTime<Utc>,
    body_pretty: &str,
) -> Option<String> {
    if CLEWDR_CONFIG.load().no_fs {
        return None;
    }
    let stamp = started_at.format("%Y%m%dT%H%M%S%.3fZ");
    let rel_path = format!(
        "probe-dumps/{}-account-{}-{}.json",
        request_type.as_str(),
        account_id,
        stamp
    );
    print_out_text(body_pretty.to_string(), &rel_path);
    Some(rel_path)
}

fn insert_if_present(dst: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value.filter(|v| !v.is_null()) {
        dst.insert(key.to_string(), value.clone());
    }
}

fn summarize_cookie_bootstrap(bootstrap: &Value, derived_account_type: &str) -> Value {
    let mut summary = Map::new();

    let account = bootstrap.get("account").and_then(Value::as_object);
    if let Some(account) = account {
        let mut account_summary = Map::new();
        insert_if_present(
            &mut account_summary,
            "email_address",
            account.get("email_address"),
        );
        insert_if_present(
            &mut account_summary,
            "display_name",
            account.get("display_name"),
        );
        insert_if_present(&mut account_summary, "full_name", account.get("full_name"));
        insert_if_present(
            &mut account_summary,
            "created_at",
            account.get("created_at"),
        );
        insert_if_present(
            &mut account_summary,
            "updated_at",
            account.get("updated_at"),
        );
        insert_if_present(&mut account_summary, "uuid", account.get("uuid"));
        insert_if_present(
            &mut account_summary,
            "is_verified",
            account.get("is_verified"),
        );
        if !account_summary.is_empty() {
            summary.insert("account".to_string(), Value::Object(account_summary));
        }
    }

    let memberships = account
        .and_then(|acc| acc.get("memberships"))
        .and_then(Value::as_array);
    let selected_membership = memberships.and_then(|memberships| {
        memberships.iter().find(|membership| {
            membership["organization"]["capabilities"]
                .as_array()
                .is_some_and(|caps| caps.iter().any(|cap| cap.as_str() == Some("chat")))
        })
    });

    if let Some(selected_membership) = selected_membership.and_then(Value::as_object) {
        let mut membership_summary = Map::new();
        insert_if_present(
            &mut membership_summary,
            "role",
            selected_membership.get("role"),
        );
        insert_if_present(
            &mut membership_summary,
            "created_at",
            selected_membership.get("created_at"),
        );

        if let Some(organization) = selected_membership
            .get("organization")
            .and_then(Value::as_object)
        {
            let mut organization_summary = Map::new();
            insert_if_present(&mut organization_summary, "uuid", organization.get("uuid"));
            insert_if_present(&mut organization_summary, "name", organization.get("name"));
            insert_if_present(
                &mut organization_summary,
                "billing_type",
                organization.get("billing_type"),
            );
            insert_if_present(
                &mut organization_summary,
                "rate_limit_tier",
                organization.get("rate_limit_tier"),
            );
            insert_if_present(
                &mut organization_summary,
                "merchant_of_record",
                organization.get("merchant_of_record"),
            );
            insert_if_present(
                &mut organization_summary,
                "free_credits_status",
                organization.get("free_credits_status"),
            );
            insert_if_present(
                &mut organization_summary,
                "capabilities",
                organization.get("capabilities"),
            );
            if !organization_summary.is_empty() {
                membership_summary.insert(
                    "organization".to_string(),
                    Value::Object(organization_summary),
                );
            }
        }

        if !membership_summary.is_empty() {
            summary.insert(
                "selected_membership".to_string(),
                Value::Object(membership_summary),
            );
        }
    }

    let mut derived = Map::new();
    derived.insert(
        "account_type".to_string(),
        Value::String(derived_account_type.to_string()),
    );
    derived.insert(
        "memberships_count".to_string(),
        Value::from(memberships.map(|v| v.len()).unwrap_or_default() as u64),
    );
    summary.insert("derived".to_string(), Value::Object(derived));

    Value::Object(summary)
}

/// Propagate InvalidCookie errors, treat everything else as transient.
fn extract_cookie_reason(e: &ClewdrError) -> Option<Reason> {
    if let ClewdrError::InvalidCookie { reason } = e {
        Some(reason.clone())
    } else {
        None
    }
}

fn is_oauth_auth_failure(err: &ClewdrError) -> bool {
    super::is_oauth_auth_failure(err)
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
        let mut truncated = Map::new();
        truncated.insert("truncated".to_string(), Value::Bool(true));
        truncated.insert("bytes".to_string(), Value::from(body.len() as u64));
        truncated.insert(
            "component_bytes".to_string(),
            Value::Object(probe_bundle_component_sizes(bundle)),
        );
        insert_if_present(
            &mut truncated,
            "debug_dump_file",
            bundle.get("debug_dump_file"),
        );
        insert_if_present(
            &mut truncated,
            "debug_component_bytes",
            bundle.get("debug_component_bytes"),
        );
        body = Value::Object(truncated).to_string();
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
    let debug_cookie = CLEWDR_CONFIG.load().debug_cookie;
    let mut debug_raw_bundle = debug_cookie.then(Map::new);

    let outcome: Result<(), ProbeFailure> = run_cookie_probe(
        account_id,
        cookie,
        handle.clone(),
        profile,
        &db,
        &mut bundle,
        debug_raw_bundle.as_mut(),
    )
    .await;

    if let Err(ref failure) = outcome {
        bundle.insert("error".into(), failure.to_bundle_entry());
    }

    if log_sink.is_some()
        && let Some(debug_raw_bundle) = debug_raw_bundle
            .as_ref()
            .filter(|bundle| !bundle.is_empty())
    {
        let body_pretty =
            serde_json::to_string_pretty(debug_raw_bundle).unwrap_or_else(|_| "{}".to_string());
        if let Some(dump_file) = dump_probe_bundle(
            RequestType::ProbeCookie,
            account_id,
            started_at,
            &body_pretty,
        ) {
            bundle.insert("debug_dump_file".into(), Value::String(dump_file));
            bundle.insert(
                "debug_component_bytes".into(),
                Value::Object(probe_bundle_component_sizes(debug_raw_bundle)),
            );
        }
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
    mut debug_raw_bundle: Option<&mut Map<String, Value>>,
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
            bundle.insert(
                "bootstrap_summary".into(),
                summarize_cookie_bootstrap(&raw, &info.account_type),
            );
            if let Some(debug_raw_bundle) = debug_raw_bundle.as_mut() {
                debug_raw_bundle.insert("bootstrap".into(), raw);
            }
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

    // 2. Persist metadata to DB (guarded by credential fingerprint to prevent stale writes)
    if let Err(e) = update_account_metadata(
        db,
        account_id,
        Some(&info.email),
        Some(&info.account_type),
        Some(&info.org_uuid),
        "cookie",
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
            if let Some(debug_raw_bundle) = debug_raw_bundle.as_mut() {
                debug_raw_bundle.insert("usage".into(), usage.clone());
            }
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

    let Some(fallback_token) = account.oauth_token.clone() else {
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

    // Serialize refreshes for this account so concurrent probes / chat
    // refreshes don't burn the single-use refresh token twice.
    let _guard = crate::services::oauth_refresh_guard::guard()
        .lock(account_id)
        .await;

    // After acquiring the guard, pick the authoritative current token. Prefer
    // the pool's in-memory copy (fast, matches dispatch-time state). If the
    // pool has no entry — typically because the account sits in
    // `state.invalid` after a prior auth_error, which is exactly where admin-
    // triggered probes retry from — fall back to a fresh DB read. DB is
    // authoritative for credentials (docs §"容易漏掉 #5"); the `fallback_token`
    // clone loaded before the guard may be stale if a peer rotated the token
    // while we were queued.
    let token = if let Some(t) = handle.get_token(account_id).await.unwrap_or(None) {
        t
    } else {
        match get_account_by_id(db, account_id).await {
            Ok(Some(acc)) => acc.oauth_token.unwrap_or(fallback_token),
            _ => fallback_token,
        }
    };

    // Fingerprint the access token this probe started with. Used on the
    // upstream-failure path to decide whether a stale probe's verdict
    // should still be allowed to stamp auth_error on the account: if the
    // credential has rotated (admin reconnect, peer refresh), the probe's
    // failure reflects the old credential and must not taint the new one.
    let starting_access_prefix = {
        let cap = 20.min(token.access_token.len());
        token.access_token[..cap].to_string()
    };

    // A probe's job is to refresh the account's health signal (profile + usage
    // + metadata), not to force a refresh-token rotation. If the current access
    // token is still fresh, we fetch the snapshot directly without rotating —
    // this avoids burning a refresh-token cycle on healthy accounts while
    // still populating the bundle, updating metadata, and driving
    // auth_error → active reactivation.
    let (refreshed_token, profile_raw, usage_raw, snapshot) = if token.is_expired() {
        match refresh_oauth_token_with_raw(&token, account.proxy_url.as_deref()).await {
            Ok((refreshed, profile_raw, usage_raw)) => (
                Some(refreshed.token),
                profile_raw,
                usage_raw,
                refreshed.snapshot,
            ),
            Err(err) => {
                return probe_oauth_upstream_failure(
                    &handle,
                    db,
                    account_id,
                    &starting_access_prefix,
                    err,
                )
                .await;
            }
        }
    } else {
        match fetch_oauth_snapshot_raw(&token.access_token, account.proxy_url.as_deref()).await {
            Ok((snapshot, profile_raw, usage_raw)) => (None, profile_raw, usage_raw, snapshot),
            Err(err) => {
                return probe_oauth_upstream_failure(
                    &handle,
                    db,
                    account_id,
                    &starting_access_prefix,
                    err,
                )
                .await;
            }
        }
    };

    bundle.insert("profile".into(), profile_raw);
    bundle.insert("usage".into(), usage_raw);

    // The access_token the DB currently holds after any refresh this probe
    // just performed. Used below as the credential fingerprint guarding the
    // metadata write — blocks stale probes after an admin reconnect or a
    // concurrent refresh rotates `oauth_access_token`.
    let authoritative_access_token: String = refreshed_token
        .as_ref()
        .map(|t| t.access_token.clone())
        .unwrap_or_else(|| token.access_token.clone());

    if let Some(new_token) = refreshed_token {
        if let Err(err) = upsert_account_oauth(db, account_id, Some(&new_token), None).await {
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
        // Mirror the freshly-rotated token into the pool so subsequent
        // dispatches and flushes don't fall back to the now-invalid RT.
        handle.update_credential(account_id, Some(new_token)).await;
    }

    let access_prefix = &authoritative_access_token[..20.min(authoritative_access_token.len())];
    if let Err(err) = update_account_metadata(
        db,
        account_id,
        snapshot.email.as_deref(),
        snapshot.account_type.as_deref(),
        Some(snapshot.organization_uuid.as_str()),
        "oauth",
        access_prefix,
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

    // Final fingerprint check before the remaining unguarded writes
    // (runtime upsert, optional set_account_active, probe-error bookkeeping).
    // `update_account_metadata` has its own guard, but once past that the
    // chain has multiple DB writes and a pool reload — a credential
    // rotation in the middle would otherwise let this stale probe reactivate
    // the account under the new credential or clobber its runtime. Aborting
    // here is intentionally coarse: we accept last-writer-wins between this
    // check and the three writes immediately following, since those all
    // happen in rapid succession on the same task.
    match account_credential_matches_prefix(db, account_id, "oauth", access_prefix).await {
        Ok(true) => {}
        Ok(false) => {
            info!(
                "[probe][oauth] account {account_id}: credential rotated during probe; abandoning remaining commits"
            );
            handle.clear_probe_error(account_id).await;
            let _ = handle.clear_probing(account_id).await;
            return Ok(());
        }
        Err(err) => {
            let msg = format!("credential fingerprint check failed: {err}");
            warn!("[probe][oauth] account {account_id}: {msg}");
            handle
                .set_probe_error(account_id, format!("OAuth probe failed: {msg}"))
                .await;
            let _ = handle.clear_probing(account_id).await;
            return Err(ProbeFailure {
                stage: "fingerprint_check",
                message: msg,
                http_status: None,
                is_auth: false,
            });
        }
    }

    if let Err(err) =
        batch_upsert_runtime_states(db, &[(account_id, snapshot.runtime.clone())]).await
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
    let did_reactivate = if account.status == "auth_error" {
        match set_account_active(db, account_id).await {
            Ok(()) => true,
            Err(err) => {
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
        }
    } else {
        false
    };
    handle.clear_probe_error(account_id).await;
    let _ = handle.clear_probing(account_id).await;
    if did_reactivate {
        // DB just flipped from auth_error back to active; the in-memory pool
        // still has this account in `state.invalid` (put there by an earlier
        // `converge_invalidate`). Trigger a reload so the account re-enters
        // the dispatchable set without waiting for the next manual reload —
        // without this step, DB says "active" while the pool refuses to
        // dispatch, which is exactly the "two sources of truth" divergence
        // the normalization doc warns against.
        let _ = handle.reload_from_db().await;
    }
    info!("[probe][oauth] completed for account {account_id}");
    Ok(())
}

/// Common tail for any upstream OAuth call failure inside `run_oauth_probe`
/// (either `refresh_oauth_token_with_raw` or `fetch_oauth_snapshot_raw`).
/// Handles auth-error DB flip, probe-error bookkeeping, probing flag clear,
/// and constructs the `ProbeFailure` with the right auth / http fields.
///
/// `expected_prefix` is the OAuth access-token fingerprint this probe
/// started with. If a concurrent admin reconnect or peer refresh has
/// rotated the credential while this probe was in flight, we skip the
/// DB auth-error flip entirely — the stale probe's failure reflects the
/// old credential, not the new one now on the account.
async fn probe_oauth_upstream_failure(
    handle: &AccountPoolHandle,
    db: &SqlitePool,
    account_id: i64,
    expected_prefix: &str,
    err: ClewdrError,
) -> Result<(), ProbeFailure> {
    let msg = err.to_string();
    warn!("[probe][oauth] account {account_id}: {msg}");
    let auth = is_oauth_auth_failure(&err);
    let still_current =
        match account_credential_matches_prefix(db, account_id, "oauth", expected_prefix).await {
            Ok(v) => v,
            Err(db_err) => {
                warn!("[probe][oauth] account {account_id}: fingerprint check failed: {db_err}");
                // Be conservative: treat as "not current" so a DB hiccup doesn't
                // let a stale probe stamp auth_error onto a rotated credential.
                false
            }
        };

    if auth && still_current {
        match set_account_auth_error(db, account_id, &msg).await {
            Ok(()) => {
                // DB is authoritative; only after the status write succeeds
                // do we converge the pool's in-memory view. Mirrors chat.rs's
                // mark_oauth_account_auth_error pattern so a transient DB
                // error can't leave the pool with a stale "invalidated" view
                // while the DB still reports the account as active.
                handle.invalidate(account_id, Reason::Null).await;
                handle.clear_probe_error(account_id).await;
            }
            Err(db_err) => {
                warn!("[probe][oauth] failed to set auth_error for account {account_id}: {db_err}");
                handle
                    .set_probe_error(account_id, format!("OAuth probe failed: {msg}"))
                    .await;
            }
        }
    } else if auth {
        // Credential rotated while probe was in flight — stale failure
        // must not taint the new credential's state. Clear any probe
        // error bookkeeping so the rotated credential gets a clean slate.
        info!(
            "[probe][oauth] account {account_id}: credential rotated during probe; skipping auth_error on stale result"
        );
        handle.clear_probe_error(account_id).await;
    } else if still_current {
        handle
            .set_probe_error(account_id, format!("OAuth probe failed: {msg}"))
            .await;
    } else {
        // Transient failure (5xx / network hiccup) on a probe whose
        // credential has since been rotated. The error belongs to the
        // old credential; showing it as the new credential's
        // `last_probe_error` would be misleading. Drop it and clear any
        // lingering entry.
        info!(
            "[probe][oauth] account {account_id}: credential rotated during probe; dropping transient probe error on stale result"
        );
        handle.clear_probe_error(account_id).await;
    }
    let _ = handle.clear_probing(account_id).await;
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
