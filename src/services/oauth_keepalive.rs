use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use tokio::time;
use tracing::{info, warn};

use crate::{
    config::{Reason, TokenInfo},
    db::accounts::{
        AccountMetadataUpdate, AccountWithRuntime, account_credential_matches_prefix,
        get_account_by_id, load_all_accounts, set_account_auth_error, set_account_disabled,
        set_account_last_failure, update_account_metadata, upsert_account_oauth,
        upsert_oauth_snapshot_runtime_fields,
    },
    error::ClewdrError,
    oauth::{OAuthAccountSnapshot, fetch_oauth_snapshot, refresh_oauth_token_only},
    services::{
        account_error::{
            AccountFailureAction, AccountFailureContextPersisted, FailureSource,
            classify_account_failure,
        },
        account_pool::{AccountPoolHandle, CredentialFingerprint},
    },
};

const SCAN_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const EXPIRY_REFRESH_WINDOW: chrono::Duration = chrono::Duration::hours(12);
const MAX_REFRESH_AGE: chrono::Duration = chrono::Duration::days(7);

type TokenRefreshFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TokenInfo, ClewdrError>> + Send + 'a>>;
type TokenRefreshFn =
    dyn for<'a> Fn(&'a TokenInfo, Option<&'a str>) -> TokenRefreshFuture<'a> + Send + Sync;
type SnapshotFuture<'a> =
    Pin<Box<dyn Future<Output = Result<OAuthAccountSnapshot, ClewdrError>> + Send + 'a>>;
type SnapshotFn = dyn for<'a> Fn(&'a str, Option<&'a str>) -> SnapshotFuture<'a> + Send + Sync;

/// Start the lightweight OAuth keepalive loop.
///
/// The first scan is spawned immediately, then repeated once per day.
pub fn start(db: SqlitePool, account_pool: AccountPoolHandle) {
    let refresher: Arc<TokenRefreshFn> = Arc::new(|token, proxy_url| {
        Box::pin(async move { refresh_oauth_token_only(token, proxy_url).await })
    });
    let snapshot_fetcher: Arc<SnapshotFn> = Arc::new(|access_token, proxy_url| {
        Box::pin(async move { fetch_oauth_snapshot(access_token, proxy_url).await })
    });

    tokio::spawn(async move {
        run_once_with_refresher(
            &db,
            &account_pool,
            refresher.clone(),
            snapshot_fetcher.clone(),
        )
        .await;

        let mut interval = time::interval(SCAN_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            run_once_with_refresher(
                &db,
                &account_pool,
                refresher.clone(),
                snapshot_fetcher.clone(),
            )
            .await;
        }
    });
}

async fn run_once_with_refresher(
    db: &SqlitePool,
    account_pool: &AccountPoolHandle,
    refresher: Arc<TokenRefreshFn>,
    snapshot_fetcher: Arc<SnapshotFn>,
) {
    let accounts = match load_all_accounts(db).await {
        Ok(accounts) => accounts,
        Err(err) => {
            warn!("[oauth-keepalive] failed to load accounts: {err}");
            return;
        }
    };

    let candidates = accounts
        .into_iter()
        .filter(is_oauth_keepalive_candidate)
        .filter(|account| should_refresh_oauth_keepalive(account, Utc::now()))
        .collect::<Vec<_>>();

    if !candidates.is_empty() {
        info!(
            "[oauth-keepalive] refreshing {} OAuth account(s)",
            candidates.len()
        );
    }

    for account in candidates {
        refresh_account(
            db,
            account_pool,
            account.id,
            refresher.clone(),
            snapshot_fetcher.clone(),
        )
        .await;
    }
}

fn is_oauth_keepalive_candidate(account: &AccountWithRuntime) -> bool {
    account.auth_source == "oauth" && account.status != "disabled" && account.oauth_token.is_some()
}

pub(crate) fn should_refresh_oauth_keepalive(
    account: &AccountWithRuntime,
    now: DateTime<Utc>,
) -> bool {
    if !is_oauth_keepalive_candidate(account) {
        return false;
    }

    let Some(token) = account.oauth_token.as_ref() else {
        return false;
    };
    if token.expires_at <= now + EXPIRY_REFRESH_WINDOW {
        return true;
    }

    let Some(last_refresh_at) = account
        .last_refresh_at
        .as_deref()
        .and_then(parse_rfc3339_utc)
    else {
        return true;
    };

    last_refresh_at <= now - MAX_REFRESH_AGE
}

fn parse_rfc3339_utc(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

async fn refresh_account(
    db: &SqlitePool,
    account_pool: &AccountPoolHandle,
    account_id: i64,
    refresher: Arc<TokenRefreshFn>,
    snapshot_fetcher: Arc<SnapshotFn>,
) {
    let _guard = crate::services::oauth_refresh_guard::guard()
        .lock(account_id)
        .await;

    let account = match get_account_by_id(db, account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return,
        Err(err) => {
            warn!("[oauth-keepalive] account {account_id}: failed to reload account: {err}");
            return;
        }
    };

    if !should_refresh_oauth_keepalive(&account, Utc::now()) {
        return;
    }

    let Some(token) = account.oauth_token.as_ref() else {
        return;
    };
    let expected_refresh_token = token.refresh_token.clone();

    match refresher(token, account.proxy_url.as_deref()).await {
        Ok(refreshed_token) => {
            match credential_still_current(db, account_id, &expected_refresh_token).await {
                Ok(true) => {}
                Ok(false) => {
                    info!(
                        "[oauth-keepalive] account {account_id}: credential rotated during refresh; dropping stale keepalive result"
                    );
                    return;
                }
                Err(err) => {
                    warn!(
                        "[oauth-keepalive] account {account_id}: failed credential check before persist: {err}"
                    );
                    return;
                }
            }

            if let Err(err) =
                persist_refreshed_token(db, account_pool, account_id, &refreshed_token).await
            {
                warn!("[oauth-keepalive] account {account_id}: failed to persist token: {err}");
                return;
            }

            match snapshot_fetcher(&refreshed_token.access_token, account.proxy_url.as_deref())
                .await
            {
                Ok(snapshot) => {
                    persist_snapshot(db, account_pool, account_id, &refreshed_token, snapshot)
                        .await;
                }
                Err(err) => {
                    persist_failure(
                        db,
                        account_pool,
                        account_id,
                        &refreshed_token.refresh_token,
                        err,
                    )
                    .await;
                }
            }
        }
        Err(err) => {
            persist_failure(db, account_pool, account_id, &expected_refresh_token, err).await
        }
    }
}

async fn credential_still_current(
    db: &SqlitePool,
    account_id: i64,
    expected_refresh_token: &str,
) -> Result<bool, sqlx::Error> {
    Ok(get_account_by_id(db, account_id)
        .await?
        .and_then(|account| account.oauth_token)
        .is_some_and(|token| token.refresh_token == expected_refresh_token))
}

async fn persist_refreshed_token(
    db: &SqlitePool,
    account_pool: &AccountPoolHandle,
    account_id: i64,
    token: &TokenInfo,
) -> Result<(), ClewdrError> {
    upsert_account_oauth(db, account_id, Some(token), None).await?;
    account_pool
        .update_credential(account_id, Some(token.clone()))
        .await;
    Ok(())
}

async fn persist_snapshot(
    db: &SqlitePool,
    account_pool: &AccountPoolHandle,
    account_id: i64,
    token: &TokenInfo,
    snapshot: OAuthAccountSnapshot,
) {
    let access_prefix = &token.access_token[..20.min(token.access_token.len())];
    if let Err(err) = update_account_metadata(
        db,
        account_id,
        AccountMetadataUpdate {
            email: snapshot.email.as_deref(),
            account_type: snapshot.account_type.as_deref(),
            organization_uuid: Some(snapshot.organization_uuid.as_str()),
            rate_limit_tier: snapshot.rate_limit_tier.as_deref(),
            subscription_created_at: snapshot.subscription_created_at.as_deref(),
            billing_type: snapshot.billing_type.as_deref(),
        },
        "oauth",
        access_prefix,
    )
    .await
    {
        warn!("[oauth-keepalive] account {account_id}: failed to persist metadata: {err}");
    }

    match account_credential_matches_prefix(db, account_id, "oauth", access_prefix).await {
        Ok(true) => {}
        Ok(false) => {
            info!(
                "[oauth-keepalive] account {account_id}: credential rotated during snapshot; dropping stale runtime"
            );
            return;
        }
        Err(err) => {
            warn!(
                "[oauth-keepalive] account {account_id}: failed snapshot credential check: {err}"
            );
            return;
        }
    }

    if let Err(err) = upsert_oauth_snapshot_runtime_fields(db, account_id, &snapshot.runtime).await
    {
        warn!("[oauth-keepalive] account {account_id}: failed to persist runtime: {err}");
    }

    if let Err(err) = account_pool
        .release_oauth_snapshot_runtime(
            account_id,
            snapshot.runtime,
            Some(CredentialFingerprint::from_oauth_refresh_token(
                &token.refresh_token,
            )),
        )
        .await
    {
        warn!("[oauth-keepalive] account {account_id}: failed to sync runtime into pool: {err}");
    }
}

async fn persist_failure(
    db: &SqlitePool,
    account_pool: &AccountPoolHandle,
    account_id: i64,
    expected_refresh_token: &str,
    err: ClewdrError,
) {
    let msg = err.to_string();
    let context = classify_account_failure(&err, FailureSource::OauthRefresh, Some("refresh"));
    warn!("[oauth-keepalive] account {account_id}: {msg}");

    if !matches!(
        context.action,
        AccountFailureAction::TerminalAuth | AccountFailureAction::TerminalDisabled
    ) {
        return;
    }

    match credential_still_current(db, account_id, expected_refresh_token).await {
        Ok(true) => {}
        Ok(false) => {
            info!(
                "[oauth-keepalive] account {account_id}: credential rotated during failure; dropping stale keepalive verdict"
            );
            return;
        }
        Err(db_err) => {
            warn!(
                "[oauth-keepalive] account {account_id}: failed credential check before failure persist: {db_err}"
            );
            return;
        }
    }

    let persisted = AccountFailureContextPersisted::from(&context);
    match context.action {
        AccountFailureAction::TerminalAuth => {
            if let Err(db_err) = set_account_auth_error(db, account_id, &msg).await {
                warn!("[oauth-keepalive] account {account_id}: failed to set auth_error: {db_err}");
                return;
            }
            if let Err(db_err) = set_account_last_failure(db, account_id, Some(&persisted)).await {
                warn!(
                    "[oauth-keepalive] account {account_id}: failed to persist last_failure: {db_err}"
                );
            }
            account_pool.invalidate(account_id, Reason::Null).await;
        }
        AccountFailureAction::TerminalDisabled => {
            let reason = context
                .normalized_reason
                .to_reason()
                .unwrap_or(Reason::Disabled);
            if let Err(db_err) = set_account_disabled(db, account_id, &reason.to_db_string()).await
            {
                warn!("[oauth-keepalive] account {account_id}: failed to set disabled: {db_err}");
                return;
            }
            if let Err(db_err) = set_account_last_failure(db, account_id, Some(&persisted)).await {
                warn!(
                    "[oauth-keepalive] account {account_id}: failed to persist last_failure: {db_err}"
                );
            }
            account_pool.invalidate(account_id, reason).await;
        }
        AccountFailureAction::Cooldown { .. }
        | AccountFailureAction::TransientUpstream
        | AccountFailureAction::InternalError => {}
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use chrono::TimeZone;
    use sqlx::Row;
    use tokio::sync::broadcast;

    use super::*;
    use crate::{
        config::{RuntimeStateParams, UsageBreakdown},
        db::init_pool,
        oauth::OAuthAccountSnapshot,
        state::AdminEvent,
    };

    fn token_with_expiry(expires_at: DateTime<Utc>, refresh: &str) -> TokenInfo {
        TokenInfo {
            access_token: format!("at-{refresh}"),
            refresh_token: refresh.to_string(),
            expires_in: expires_at
                .signed_duration_since(Utc::now())
                .to_std()
                .unwrap_or_default(),
            expires_at,
            organization: crate::config::Organization {
                uuid: "org-test".to_string(),
            },
        }
    }

    fn runtime() -> RuntimeStateParams {
        RuntimeStateParams {
            reset_time: Some(123),
            supports_claude_1m_sonnet: None,
            supports_claude_1m_opus: None,
            count_tokens_allowed: None,
            session_resets_at: Some(456),
            weekly_resets_at: None,
            weekly_sonnet_resets_at: None,
            weekly_opus_resets_at: None,
            resets_last_checked_at: Some(789),
            session_has_reset: Some(true),
            weekly_has_reset: None,
            weekly_sonnet_has_reset: None,
            weekly_opus_has_reset: None,
            session_utilization: Some(0.25),
            weekly_utilization: None,
            weekly_sonnet_utilization: None,
            weekly_opus_utilization: None,
            buckets: std::array::from_fn(|_| UsageBreakdown::default()),
        }
    }

    fn account(
        auth_source: &str,
        status: &str,
        token: Option<TokenInfo>,
        last_refresh_at: Option<DateTime<Utc>>,
    ) -> AccountWithRuntime {
        AccountWithRuntime {
            id: 1,
            name: "test".to_string(),
            rr_order: 1,
            max_slots: 1,
            proxy_id: None,
            proxy_name: None,
            proxy_url: None,
            drain_first: false,
            status: status.to_string(),
            auth_source: auth_source.to_string(),
            cookie_blob: None,
            oauth_expires_at: token.as_ref().map(|t| t.expires_at.to_rfc3339()),
            oauth_token: token,
            last_refresh_at: last_refresh_at.map(|dt| dt.to_rfc3339()),
            last_error: None,
            organization_uuid: Some("org-test".to_string()),
            invalid_reason: None,
            last_failure: None,
            email: None,
            account_type: None,
            rate_limit_tier: None,
            subscription_created_at: None,
            billing_type: None,
            api_key_base_url: None,
            api_key_secret: None,
            api_key_extra_headers: None,
            created_at: None,
            updated_at: None,
            runtime: None,
        }
    }

    async fn setup_pool() -> (SqlitePool, AccountPoolHandle) {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        let (event_tx, _) = broadcast::channel::<AdminEvent>(16);
        let handle = AccountPoolHandle::start(pool.clone(), event_tx)
            .await
            .unwrap();
        (pool, handle)
    }

    async fn insert_oauth_account(
        pool: &SqlitePool,
        id: i64,
        status: &str,
        access: &str,
        refresh: &str,
        expires_at: DateTime<Utc>,
        last_refresh_at: Option<DateTime<Utc>>,
    ) {
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source,
                oauth_access_token, oauth_refresh_token, oauth_expires_at,
                organization_uuid, last_refresh_at, drain_first
            ) VALUES (?1, ?2, ?3, 1, ?4, 'oauth', ?5, ?6, ?7, 'org-test', ?8, 0)",
        )
        .bind(id)
        .bind(format!("oauth-{id}"))
        .bind(id)
        .bind(status)
        .bind(access)
        .bind(refresh)
        .bind(expires_at.to_rfc3339())
        .bind(last_refresh_at.map(|dt| dt.to_rfc3339()))
        .execute(pool)
        .await
        .unwrap();
    }

    fn success_refresher(call_count: Arc<AtomicUsize>) -> Arc<TokenRefreshFn> {
        Arc::new(move |token, _proxy_url| {
            let call_count = Arc::clone(&call_count);
            let refresh = token.refresh_token.clone();
            Box::pin(async move {
                call_count.fetch_add(1, Ordering::SeqCst);
                Ok(TokenInfo::from_parts(
                    "at-new".to_string(),
                    format!("{refresh}-new"),
                    Duration::from_secs(3600),
                    "org-new".to_string(),
                ))
            })
        })
    }

    fn success_snapshot_fetcher() -> Arc<SnapshotFn> {
        Arc::new(|_, _| {
            Box::pin(async {
                Ok(OAuthAccountSnapshot {
                    email: Some("user@example.com".to_string()),
                    account_type: Some("max".to_string()),
                    organization_uuid: "org-new".to_string(),
                    rate_limit_tier: Some("tier".to_string()),
                    billing_type: Some("billing".to_string()),
                    subscription_created_at: Some("2026-01-01T00:00:00Z".to_string()),
                    runtime: runtime(),
                })
            })
        })
    }

    #[test]
    fn should_refresh_oauth_keepalive_matrix() {
        let now = Utc.with_ymd_and_hms(2026, 5, 19, 12, 0, 0).unwrap();
        let fresh = token_with_expiry(now + chrono::Duration::days(2), "rt-fresh");

        assert!(should_refresh_oauth_keepalive(
            &account(
                "oauth",
                "active",
                Some(token_with_expiry(
                    now - chrono::Duration::minutes(1),
                    "rt-expired"
                )),
                Some(now)
            ),
            now
        ));
        assert!(should_refresh_oauth_keepalive(
            &account(
                "oauth",
                "active",
                Some(token_with_expiry(
                    now + chrono::Duration::hours(6),
                    "rt-soon"
                )),
                Some(now)
            ),
            now
        ));
        assert!(should_refresh_oauth_keepalive(
            &account("oauth", "active", Some(fresh.clone()), None),
            now
        ));
        assert!(should_refresh_oauth_keepalive(
            &account(
                "oauth",
                "active",
                Some(fresh.clone()),
                Some(now - chrono::Duration::days(8))
            ),
            now
        ));
        assert!(!should_refresh_oauth_keepalive(
            &account("oauth", "active", Some(fresh.clone()), Some(now)),
            now
        ));
        assert!(!should_refresh_oauth_keepalive(
            &account("cookie", "active", Some(fresh.clone()), None),
            now
        ));
        assert!(!should_refresh_oauth_keepalive(
            &account("oauth", "disabled", Some(fresh.clone()), None),
            now
        ));
        assert!(!should_refresh_oauth_keepalive(
            &account("oauth", "active", None, None),
            now
        ));
    }

    #[tokio::test]
    async fn lock_reread_skips_when_peer_already_refreshed() {
        let (pool, handle) = setup_pool().await;
        let now = Utc::now();
        insert_oauth_account(
            &pool,
            1,
            "active",
            "at-old",
            "rt-old",
            now + chrono::Duration::hours(1),
            Some(now - chrono::Duration::days(8)),
        )
        .await;
        handle.reload_from_db().await.unwrap();

        upsert_account_oauth(
            &pool,
            1,
            Some(&TokenInfo::from_parts(
                "at-peer".to_string(),
                "rt-peer".to_string(),
                Duration::from_secs(48 * 60 * 60),
                "org-test".to_string(),
            )),
            None,
        )
        .await
        .unwrap();
        handle
            .update_credential(
                1,
                Some(TokenInfo::from_parts(
                    "at-peer".to_string(),
                    "rt-peer".to_string(),
                    Duration::from_secs(48 * 60 * 60),
                    "org-test".to_string(),
                )),
            )
            .await;

        let calls = Arc::new(AtomicUsize::new(0));
        refresh_account(
            &pool,
            &handle,
            1,
            success_refresher(calls.clone()),
            success_snapshot_fetcher(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn refresh_success_updates_db_and_pool() {
        let (pool, handle) = setup_pool().await;
        let now = Utc::now();
        insert_oauth_account(
            &pool,
            1,
            "active",
            "at-old",
            "rt-old",
            now + chrono::Duration::hours(1),
            Some(now - chrono::Duration::days(8)),
        )
        .await;
        handle.reload_from_db().await.unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        refresh_account(
            &pool,
            &handle,
            1,
            success_refresher(calls.clone()),
            success_snapshot_fetcher(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let row = sqlx::query(
            "SELECT oauth_access_token, oauth_refresh_token, oauth_expires_at, last_refresh_at,
                    email, account_type, organization_uuid, rate_limit_tier,
                    subscription_created_at, billing_type
             FROM accounts WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("oauth_access_token"), "at-new");
        assert_eq!(row.get::<String, _>("oauth_refresh_token"), "rt-old-new");
        assert!(row.get::<Option<String>, _>("oauth_expires_at").is_some());
        assert!(row.get::<Option<String>, _>("last_refresh_at").is_some());
        assert_eq!(row.get::<String, _>("email"), "user@example.com");
        assert_eq!(row.get::<String, _>("account_type"), "max");
        assert_eq!(row.get::<String, _>("organization_uuid"), "org-new");
        assert_eq!(row.get::<String, _>("rate_limit_tier"), "tier");
        assert_eq!(
            row.get::<String, _>("subscription_created_at"),
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(row.get::<String, _>("billing_type"), "billing");

        let token = handle.get_token(1).await.unwrap().unwrap();
        assert_eq!(token.access_token, "at-new");
        assert_eq!(token.refresh_token, "rt-old-new");
    }

    #[tokio::test]
    async fn invalid_grant_sets_auth_error() {
        let (pool, handle) = setup_pool().await;
        let now = Utc::now();
        insert_oauth_account(
            &pool,
            1,
            "active",
            "at-old",
            "rt-old",
            now + chrono::Duration::hours(1),
            Some(now - chrono::Duration::days(8)),
        )
        .await;
        handle.reload_from_db().await.unwrap();

        let refresher: Arc<TokenRefreshFn> = Arc::new(|_, _| {
            Box::pin(async {
                Err(ClewdrError::Whatever {
                    message: "oauth refresh failed: invalid_grant".to_string(),
                    source: None,
                })
            })
        });
        refresh_account(&pool, &handle, 1, refresher, success_snapshot_fetcher()).await;

        let row =
            sqlx::query("SELECT status, last_error, last_failure_json FROM accounts WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.get::<String, _>("status"), "auth_error");
        assert!(row.get::<String, _>("last_error").contains("invalid_grant"));
        assert!(
            row.get::<String, _>("last_failure_json")
                .contains("oauth_refresh_invalid")
        );
    }

    #[tokio::test]
    async fn transient_failure_does_not_change_status() {
        let (pool, handle) = setup_pool().await;
        let now = Utc::now();
        insert_oauth_account(
            &pool,
            1,
            "active",
            "at-old",
            "rt-old",
            now + chrono::Duration::hours(1),
            Some(now - chrono::Duration::days(8)),
        )
        .await;
        handle.reload_from_db().await.unwrap();

        let refresher: Arc<TokenRefreshFn> = Arc::new(|_, _| {
            Box::pin(async {
                Err(ClewdrError::Whatever {
                    message: "OAuth token request failed with status 500: upstream".to_string(),
                    source: None,
                })
            })
        });
        refresh_account(&pool, &handle, 1, refresher, success_snapshot_fetcher()).await;

        let row =
            sqlx::query("SELECT status, last_error, last_failure_json FROM accounts WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.get::<String, _>("status"), "active");
        assert!(row.get::<Option<String>, _>("last_error").is_none());
        assert!(row.get::<Option<String>, _>("last_failure_json").is_none());
    }

    #[tokio::test]
    async fn snapshot_transient_after_refresh_keeps_rotated_token() {
        let (pool, handle) = setup_pool().await;
        let now = Utc::now();
        insert_oauth_account(
            &pool,
            1,
            "active",
            "at-old",
            "rt-old",
            now + chrono::Duration::hours(1),
            Some(now - chrono::Duration::days(8)),
        )
        .await;
        handle.reload_from_db().await.unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot_fetcher: Arc<SnapshotFn> = Arc::new(|_, _| {
            Box::pin(async {
                Err(ClewdrError::Whatever {
                    message: "OAuth snapshot failed with status 500: upstream".to_string(),
                    source: None,
                })
            })
        });
        refresh_account(
            &pool,
            &handle,
            1,
            success_refresher(calls.clone()),
            snapshot_fetcher,
        )
        .await;

        let row = sqlx::query(
            "SELECT status, oauth_access_token, oauth_refresh_token FROM accounts WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("status"), "active");
        assert_eq!(row.get::<String, _>("oauth_access_token"), "at-new");
        assert_eq!(row.get::<String, _>("oauth_refresh_token"), "rt-old-new");
    }

    #[tokio::test]
    async fn stale_success_after_credential_replacement_is_dropped() {
        let (pool, handle) = setup_pool().await;
        let now = Utc::now();
        insert_oauth_account(
            &pool,
            1,
            "active",
            "at-old",
            "rt-old",
            now + chrono::Duration::hours(1),
            Some(now - chrono::Duration::days(8)),
        )
        .await;
        handle.reload_from_db().await.unwrap();

        let pool_for_refresher = pool.clone();
        let refresher: Arc<TokenRefreshFn> = Arc::new(move |_, _| {
            let pool = pool_for_refresher.clone();
            Box::pin(async move {
                upsert_account_oauth(
                    &pool,
                    1,
                    Some(&TokenInfo::from_parts(
                        "at-admin".to_string(),
                        "rt-admin".to_string(),
                        Duration::from_secs(48 * 60 * 60),
                        "org-admin".to_string(),
                    )),
                    None,
                )
                .await
                .unwrap();
                Ok(TokenInfo::from_parts(
                    "at-stale".to_string(),
                    "rt-stale".to_string(),
                    Duration::from_secs(3600),
                    "org-old".to_string(),
                ))
            })
        });

        refresh_account(&pool, &handle, 1, refresher, success_snapshot_fetcher()).await;

        let row = sqlx::query(
            "SELECT status, oauth_access_token, oauth_refresh_token FROM accounts WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("status"), "active");
        assert_eq!(row.get::<String, _>("oauth_access_token"), "at-admin");
        assert_eq!(row.get::<String, _>("oauth_refresh_token"), "rt-admin");
    }

    #[tokio::test]
    async fn stale_invalid_grant_after_credential_replacement_is_dropped() {
        let (pool, handle) = setup_pool().await;
        let now = Utc::now();
        insert_oauth_account(
            &pool,
            1,
            "active",
            "at-old",
            "rt-old",
            now + chrono::Duration::hours(1),
            Some(now - chrono::Duration::days(8)),
        )
        .await;
        handle.reload_from_db().await.unwrap();

        let pool_for_refresher = pool.clone();
        let refresher: Arc<TokenRefreshFn> = Arc::new(move |_, _| {
            let pool = pool_for_refresher.clone();
            Box::pin(async move {
                upsert_account_oauth(
                    &pool,
                    1,
                    Some(&TokenInfo::from_parts(
                        "at-admin".to_string(),
                        "rt-admin".to_string(),
                        Duration::from_secs(48 * 60 * 60),
                        "org-admin".to_string(),
                    )),
                    None,
                )
                .await
                .unwrap();
                Err(ClewdrError::Whatever {
                    message: "oauth refresh failed: invalid_grant".to_string(),
                    source: None,
                })
            })
        });

        refresh_account(&pool, &handle, 1, refresher, success_snapshot_fetcher()).await;

        let row =
            sqlx::query("SELECT status, oauth_access_token, oauth_refresh_token, last_error FROM accounts WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.get::<String, _>("status"), "active");
        assert_eq!(row.get::<String, _>("oauth_access_token"), "at-admin");
        assert_eq!(row.get::<String, _>("oauth_refresh_token"), "rt-admin");
        assert!(row.get::<Option<String>, _>("last_error").is_none());
    }

    #[tokio::test]
    async fn terminal_disabled_snapshot_failure_disables_account() {
        let (pool, handle) = setup_pool().await;
        let now = Utc::now();
        insert_oauth_account(
            &pool,
            1,
            "active",
            "at-old",
            "rt-old",
            now + chrono::Duration::hours(1),
            Some(now - chrono::Duration::days(8)),
        )
        .await;
        handle.reload_from_db().await.unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot_fetcher: Arc<SnapshotFn> = Arc::new(|_, _| {
            Box::pin(async {
                Err(ClewdrError::InvalidCookie {
                    reason: Reason::Disabled,
                })
            })
        });
        refresh_account(
            &pool,
            &handle,
            1,
            success_refresher(calls.clone()),
            snapshot_fetcher,
        )
        .await;

        let row = sqlx::query(
            "SELECT status, invalid_reason, last_failure_json FROM accounts WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("status"), "disabled");
        assert_eq!(row.get::<String, _>("invalid_reason"), "disabled");
        assert!(
            row.get::<String, _>("last_failure_json")
                .contains("organization_disabled")
        );
    }
}
