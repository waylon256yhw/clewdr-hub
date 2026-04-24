use sqlx::{Row, SqlitePool};

use crate::config::{RuntimeStateParams, TokenInfo, UsageBreakdown};

/// Joined result of accounts + account_runtime_state.
#[derive(Debug, Clone)]
pub struct AccountWithRuntime {
    pub id: i64,
    pub name: String,
    pub rr_order: i64,
    pub max_slots: i64,
    pub proxy_id: Option<i64>,
    pub proxy_name: Option<String>,
    pub proxy_url: Option<String>,
    pub drain_first: bool,
    pub status: String,
    pub auth_source: String,
    pub cookie_blob: Option<String>,
    pub oauth_token: Option<TokenInfo>,
    pub oauth_expires_at: Option<String>,
    pub last_refresh_at: Option<String>,
    pub last_error: Option<String>,
    pub organization_uuid: Option<String>,
    pub invalid_reason: Option<String>,
    pub email: Option<String>,
    pub account_type: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub runtime: Option<RuntimeStateRow>,
}

#[derive(Debug, Clone)]
pub struct RuntimeStateRow {
    pub reset_time: Option<i64>,
    pub supports_claude_1m_sonnet: Option<bool>,
    pub supports_claude_1m_opus: Option<bool>,
    pub count_tokens_allowed: Option<bool>,
    pub session_resets_at: Option<i64>,
    pub weekly_resets_at: Option<i64>,
    pub weekly_sonnet_resets_at: Option<i64>,
    pub weekly_opus_resets_at: Option<i64>,
    pub resets_last_checked_at: Option<i64>,
    pub session_has_reset: Option<bool>,
    pub weekly_has_reset: Option<bool>,
    pub weekly_sonnet_has_reset: Option<bool>,
    pub weekly_opus_has_reset: Option<bool>,
    pub session_utilization: Option<f64>,
    pub weekly_utilization: Option<f64>,
    pub weekly_sonnet_utilization: Option<f64>,
    pub weekly_opus_utilization: Option<f64>,
    pub buckets: [UsageBreakdown; 5],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AccountPoolSummary {
    pub valid: usize,
    pub exhausted: usize,
    pub invalid: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AccountStatusSummary {
    pub active: i64,
    pub cooling: i64,
    pub error: i64,
    pub disabled: i64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AccountAuthSourceSummary {
    pub oauth: i64,
    pub cookie: i64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AccountSummary {
    pub total: i64,
    pub pool: AccountPoolSummary,
    pub statuses: AccountStatusSummary,
    pub auth_sources: AccountAuthSourceSummary,
}

impl RuntimeStateRow {
    pub fn to_params(&self) -> RuntimeStateParams {
        RuntimeStateParams {
            reset_time: self.reset_time,
            supports_claude_1m_sonnet: self.supports_claude_1m_sonnet,
            supports_claude_1m_opus: self.supports_claude_1m_opus,
            count_tokens_allowed: self.count_tokens_allowed,
            session_resets_at: self.session_resets_at,
            weekly_resets_at: self.weekly_resets_at,
            weekly_sonnet_resets_at: self.weekly_sonnet_resets_at,
            weekly_opus_resets_at: self.weekly_opus_resets_at,
            resets_last_checked_at: self.resets_last_checked_at,
            session_has_reset: self.session_has_reset,
            weekly_has_reset: self.weekly_has_reset,
            weekly_sonnet_has_reset: self.weekly_sonnet_has_reset,
            weekly_opus_has_reset: self.weekly_opus_has_reset,
            session_utilization: self.session_utilization,
            weekly_utilization: self.weekly_utilization,
            weekly_sonnet_utilization: self.weekly_sonnet_utilization,
            weekly_opus_utilization: self.weekly_opus_utilization,
            buckets: self.buckets.clone(),
        }
    }
}

fn bool_from_int(v: Option<i64>) -> Option<bool> {
    v.map(|i| i != 0)
}

pub fn active_reset_time(account: &AccountWithRuntime) -> Option<i64> {
    let now = chrono::Utc::now().timestamp();
    account
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.reset_time)
        .filter(|ts| *ts > now)
}

pub fn is_temporarily_unavailable(account: &AccountWithRuntime) -> bool {
    active_reset_time(account).is_some()
}

pub fn summarize_accounts(accounts: &[AccountWithRuntime]) -> AccountSummary {
    let mut summary = AccountSummary {
        total: accounts.len() as i64,
        ..Default::default()
    };

    for account in accounts {
        match account.auth_source.as_str() {
            "oauth" => summary.auth_sources.oauth += 1,
            "cookie" => summary.auth_sources.cookie += 1,
            _ => {}
        }

        let pool_eligible = account.cookie_blob.as_ref().is_some_and(|v| !v.is_empty())
            || (account.auth_source == "oauth" && account.oauth_token.is_some());

        match account.status.as_str() {
            "disabled" => {
                summary.statuses.disabled += 1;
                if pool_eligible {
                    summary.pool.invalid += 1;
                }
            }
            "auth_error" => {
                summary.statuses.error += 1;
                if pool_eligible {
                    summary.pool.invalid += 1;
                }
            }
            _ if is_temporarily_unavailable(account) => {
                summary.statuses.cooling += 1;
                if pool_eligible {
                    summary.pool.exhausted += 1;
                }
            }
            _ => {
                summary.statuses.active += 1;
                if pool_eligible {
                    summary.pool.valid += 1;
                }
            }
        }
    }

    summary
}

fn get_u64(row: &sqlx::sqlite::SqliteRow, col: &str) -> u64 {
    row.get::<i64, _>(col) as u64
}

fn make_bucket(row: &sqlx::sqlite::SqliteRow, prefix: &str) -> UsageBreakdown {
    UsageBreakdown {
        total_input_tokens: get_u64(row, &format!("{prefix}_total_input")),
        total_output_tokens: get_u64(row, &format!("{prefix}_total_output")),
        sonnet_input_tokens: get_u64(row, &format!("{prefix}_sonnet_input")),
        sonnet_output_tokens: get_u64(row, &format!("{prefix}_sonnet_output")),
        opus_input_tokens: get_u64(row, &format!("{prefix}_opus_input")),
        opus_output_tokens: get_u64(row, &format!("{prefix}_opus_output")),
    }
}

/// Load all accounts with their runtime state (LEFT JOIN).
pub async fn load_all_accounts(pool: &SqlitePool) -> Result<Vec<AccountWithRuntime>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT
            a.id, a.name, a.rr_order, a.max_slots, a.proxy_id,
            p.name AS proxy_name,
            p.protocol AS proxy_protocol,
            p.host AS proxy_host,
            p.port AS proxy_port,
            p.username AS proxy_username,
            p.password AS proxy_password,
            a.status, a.auth_source, a.cookie_blob,
            a.oauth_access_token, a.oauth_refresh_token, a.oauth_expires_at,
            a.organization_uuid, a.last_refresh_at, a.last_error, a.invalid_reason,
            a.email, a.account_type, a.created_at, a.updated_at,
            a.drain_first,
            rs.account_id AS rs_marker,
            rs.reset_time,
            rs.supports_claude_1m_sonnet, rs.supports_claude_1m_opus, rs.count_tokens_allowed,
            rs.session_resets_at, rs.weekly_resets_at, rs.weekly_sonnet_resets_at, rs.weekly_opus_resets_at,
            rs.resets_last_checked_at,
            rs.session_has_reset, rs.weekly_has_reset, rs.weekly_sonnet_has_reset, rs.weekly_opus_has_reset,
            rs.session_utilization, rs.weekly_utilization, rs.weekly_sonnet_utilization, rs.weekly_opus_utilization,
            COALESCE(rs.session_total_input, 0) AS session_total_input,
            COALESCE(rs.session_total_output, 0) AS session_total_output,
            COALESCE(rs.session_sonnet_input, 0) AS session_sonnet_input,
            COALESCE(rs.session_sonnet_output, 0) AS session_sonnet_output,
            COALESCE(rs.session_opus_input, 0) AS session_opus_input,
            COALESCE(rs.session_opus_output, 0) AS session_opus_output,
            COALESCE(rs.weekly_total_input, 0) AS weekly_total_input,
            COALESCE(rs.weekly_total_output, 0) AS weekly_total_output,
            COALESCE(rs.weekly_sonnet_input, 0) AS weekly_sonnet_input,
            COALESCE(rs.weekly_sonnet_output, 0) AS weekly_sonnet_output,
            COALESCE(rs.weekly_opus_input, 0) AS weekly_opus_input,
            COALESCE(rs.weekly_opus_output, 0) AS weekly_opus_output,
            COALESCE(rs.ws_total_input, 0) AS ws_total_input,
            COALESCE(rs.ws_total_output, 0) AS ws_total_output,
            COALESCE(rs.ws_sonnet_input, 0) AS ws_sonnet_input,
            COALESCE(rs.ws_sonnet_output, 0) AS ws_sonnet_output,
            COALESCE(rs.ws_opus_input, 0) AS ws_opus_input,
            COALESCE(rs.ws_opus_output, 0) AS ws_opus_output,
            COALESCE(rs.wo_total_input, 0) AS wo_total_input,
            COALESCE(rs.wo_total_output, 0) AS wo_total_output,
            COALESCE(rs.wo_sonnet_input, 0) AS wo_sonnet_input,
            COALESCE(rs.wo_sonnet_output, 0) AS wo_sonnet_output,
            COALESCE(rs.wo_opus_input, 0) AS wo_opus_input,
            COALESCE(rs.wo_opus_output, 0) AS wo_opus_output,
            COALESCE(rs.lifetime_total_input, 0) AS lifetime_total_input,
            COALESCE(rs.lifetime_total_output, 0) AS lifetime_total_output,
            COALESCE(rs.lifetime_sonnet_input, 0) AS lifetime_sonnet_input,
            COALESCE(rs.lifetime_sonnet_output, 0) AS lifetime_sonnet_output,
            COALESCE(rs.lifetime_opus_input, 0) AS lifetime_opus_input,
            COALESCE(rs.lifetime_opus_output, 0) AS lifetime_opus_output
        FROM accounts a
        LEFT JOIN proxies p ON p.id = a.proxy_id
        LEFT JOIN account_runtime_state rs ON a.id = rs.account_id
        ORDER BY a.rr_order ASC"#,
    )
    .fetch_all(pool)
    .await?;

    let mut result = Vec::with_capacity(rows.len());
    for row in &rows {
        let rs_marker: Option<i64> = row.get("rs_marker");
        let runtime = rs_marker.map(|_| RuntimeStateRow {
            reset_time: row.get("reset_time"),
            supports_claude_1m_sonnet: bool_from_int(row.get("supports_claude_1m_sonnet")),
            supports_claude_1m_opus: bool_from_int(row.get("supports_claude_1m_opus")),
            count_tokens_allowed: bool_from_int(row.get("count_tokens_allowed")),
            session_resets_at: row.get("session_resets_at"),
            weekly_resets_at: row.get("weekly_resets_at"),
            weekly_sonnet_resets_at: row.get("weekly_sonnet_resets_at"),
            weekly_opus_resets_at: row.get("weekly_opus_resets_at"),
            resets_last_checked_at: row.get("resets_last_checked_at"),
            session_has_reset: bool_from_int(row.get("session_has_reset")),
            weekly_has_reset: bool_from_int(row.get("weekly_has_reset")),
            weekly_sonnet_has_reset: bool_from_int(row.get("weekly_sonnet_has_reset")),
            weekly_opus_has_reset: bool_from_int(row.get("weekly_opus_has_reset")),
            session_utilization: row.get("session_utilization"),
            weekly_utilization: row.get("weekly_utilization"),
            weekly_sonnet_utilization: row.get("weekly_sonnet_utilization"),
            weekly_opus_utilization: row.get("weekly_opus_utilization"),
            buckets: [
                make_bucket(row, "session"),
                make_bucket(row, "weekly"),
                make_bucket(row, "ws"),
                make_bucket(row, "wo"),
                make_bucket(row, "lifetime"),
            ],
        });

        let oauth_access_token: Option<String> = row.get("oauth_access_token");
        let oauth_refresh_token: Option<String> = row.get("oauth_refresh_token");
        let oauth_expires_at: Option<String> = row.get("oauth_expires_at");
        let organization_uuid: Option<String> = row.get("organization_uuid");
        let oauth_token = match (
            oauth_access_token,
            oauth_refresh_token,
            oauth_expires_at.clone(),
            organization_uuid.clone(),
        ) {
            (Some(access_token), Some(refresh_token), Some(expires_at), Some(org_uuid)) => {
                let expires_at = chrono::DateTime::parse_from_rfc3339(&expires_at)
                    .ok()
                    .map(|dt| dt.with_timezone(&chrono::Utc));
                expires_at.map(|expires_at| TokenInfo {
                    access_token,
                    refresh_token,
                    expires_in: (expires_at - chrono::Utc::now())
                        .to_std()
                        .unwrap_or_default(),
                    expires_at,
                    organization: crate::config::Organization { uuid: org_uuid },
                })
            }
            _ => None,
        };

        let proxy_protocol: Option<String> = row.get("proxy_protocol");
        let proxy_host: Option<String> = row.get("proxy_host");
        let proxy_port: Option<i64> = row.get("proxy_port");
        let proxy_url = match (proxy_protocol.as_deref(), proxy_host.as_deref(), proxy_port) {
            (Some(protocol), Some(host), Some(port)) => {
                crate::db::proxies::build_proxy_url_from_parts(
                    protocol,
                    host,
                    port,
                    row.get::<Option<String>, _>("proxy_username").as_deref(),
                    row.get::<Option<String>, _>("proxy_password").as_deref(),
                )
                .ok()
            }
            _ => None,
        };

        result.push(AccountWithRuntime {
            id: row.get("id"),
            name: row.get("name"),
            rr_order: row.get("rr_order"),
            max_slots: row.get("max_slots"),
            proxy_id: row.get("proxy_id"),
            proxy_name: row.get("proxy_name"),
            proxy_url,
            drain_first: row.get::<i64, _>("drain_first") != 0,
            status: row.get("status"),
            auth_source: row.get("auth_source"),
            cookie_blob: row.get("cookie_blob"),
            oauth_token,
            oauth_expires_at,
            last_refresh_at: row.get("last_refresh_at"),
            last_error: row.get("last_error"),
            organization_uuid,
            invalid_reason: row.get("invalid_reason"),
            email: row.get("email"),
            account_type: row.get("account_type"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            runtime,
        });
    }
    Ok(result)
}

fn bool_to_int(v: Option<bool>) -> Option<i64> {
    v.map(|b| b as i64)
}

/// Batch upsert runtime state for multiple accounts in a single transaction.
pub async fn batch_upsert_runtime_states(
    pool: &SqlitePool,
    states: &[(i64, RuntimeStateParams)],
) -> Result<(), sqlx::Error> {
    if states.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for (account_id, p) in states {
        sqlx::query(
            r#"INSERT INTO account_runtime_state (
                account_id, reset_time,
                supports_claude_1m_sonnet, supports_claude_1m_opus, count_tokens_allowed,
                session_resets_at, weekly_resets_at, weekly_sonnet_resets_at, weekly_opus_resets_at,
                resets_last_checked_at,
                session_has_reset, weekly_has_reset, weekly_sonnet_has_reset, weekly_opus_has_reset,
                session_total_input, session_total_output, session_sonnet_input, session_sonnet_output, session_opus_input, session_opus_output,
                weekly_total_input, weekly_total_output, weekly_sonnet_input, weekly_sonnet_output, weekly_opus_input, weekly_opus_output,
                ws_total_input, ws_total_output, ws_sonnet_input, ws_sonnet_output, ws_opus_input, ws_opus_output,
                wo_total_input, wo_total_output, wo_sonnet_input, wo_sonnet_output, wo_opus_input, wo_opus_output,
                lifetime_total_input, lifetime_total_output, lifetime_sonnet_input, lifetime_sonnet_output, lifetime_opus_input, lifetime_opus_output,
                session_utilization, weekly_utilization, weekly_sonnet_utilization, weekly_opus_utilization,
                updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                ?15, ?16, ?17, ?18, ?19, ?20,
                ?21, ?22, ?23, ?24, ?25, ?26,
                ?27, ?28, ?29, ?30, ?31, ?32,
                ?33, ?34, ?35, ?36, ?37, ?38,
                ?39, ?40, ?41, ?42, ?43, ?44,
                ?45, ?46, ?47, ?48,
                CURRENT_TIMESTAMP
            ) ON CONFLICT(account_id) DO UPDATE SET
                reset_time = excluded.reset_time,
                supports_claude_1m_sonnet = excluded.supports_claude_1m_sonnet,
                supports_claude_1m_opus = excluded.supports_claude_1m_opus,
                count_tokens_allowed = excluded.count_tokens_allowed,
                session_resets_at = excluded.session_resets_at,
                weekly_resets_at = excluded.weekly_resets_at,
                weekly_sonnet_resets_at = excluded.weekly_sonnet_resets_at,
                weekly_opus_resets_at = excluded.weekly_opus_resets_at,
                resets_last_checked_at = excluded.resets_last_checked_at,
                session_has_reset = excluded.session_has_reset,
                weekly_has_reset = excluded.weekly_has_reset,
                weekly_sonnet_has_reset = excluded.weekly_sonnet_has_reset,
                weekly_opus_has_reset = excluded.weekly_opus_has_reset,
                session_total_input = excluded.session_total_input,
                session_total_output = excluded.session_total_output,
                session_sonnet_input = excluded.session_sonnet_input,
                session_sonnet_output = excluded.session_sonnet_output,
                session_opus_input = excluded.session_opus_input,
                session_opus_output = excluded.session_opus_output,
                weekly_total_input = excluded.weekly_total_input,
                weekly_total_output = excluded.weekly_total_output,
                weekly_sonnet_input = excluded.weekly_sonnet_input,
                weekly_sonnet_output = excluded.weekly_sonnet_output,
                weekly_opus_input = excluded.weekly_opus_input,
                weekly_opus_output = excluded.weekly_opus_output,
                ws_total_input = excluded.ws_total_input,
                ws_total_output = excluded.ws_total_output,
                ws_sonnet_input = excluded.ws_sonnet_input,
                ws_sonnet_output = excluded.ws_sonnet_output,
                ws_opus_input = excluded.ws_opus_input,
                ws_opus_output = excluded.ws_opus_output,
                wo_total_input = excluded.wo_total_input,
                wo_total_output = excluded.wo_total_output,
                wo_sonnet_input = excluded.wo_sonnet_input,
                wo_sonnet_output = excluded.wo_sonnet_output,
                wo_opus_input = excluded.wo_opus_input,
                wo_opus_output = excluded.wo_opus_output,
                lifetime_total_input = excluded.lifetime_total_input,
                lifetime_total_output = excluded.lifetime_total_output,
                lifetime_sonnet_input = excluded.lifetime_sonnet_input,
                lifetime_sonnet_output = excluded.lifetime_sonnet_output,
                lifetime_opus_input = excluded.lifetime_opus_input,
                lifetime_opus_output = excluded.lifetime_opus_output,
                session_utilization = excluded.session_utilization,
                weekly_utilization = excluded.weekly_utilization,
                weekly_sonnet_utilization = excluded.weekly_sonnet_utilization,
                weekly_opus_utilization = excluded.weekly_opus_utilization,
                updated_at = CURRENT_TIMESTAMP"#,
        )
        .bind(account_id)
        .bind(p.reset_time)
        .bind(bool_to_int(p.supports_claude_1m_sonnet))
        .bind(bool_to_int(p.supports_claude_1m_opus))
        .bind(bool_to_int(p.count_tokens_allowed))
        .bind(p.session_resets_at)
        .bind(p.weekly_resets_at)
        .bind(p.weekly_sonnet_resets_at)
        .bind(p.weekly_opus_resets_at)
        .bind(p.resets_last_checked_at)
        .bind(bool_to_int(p.session_has_reset))
        .bind(bool_to_int(p.weekly_has_reset))
        .bind(bool_to_int(p.weekly_sonnet_has_reset))
        .bind(bool_to_int(p.weekly_opus_has_reset))
        .bind(p.buckets[0].total_input_tokens as i64)
        .bind(p.buckets[0].total_output_tokens as i64)
        .bind(p.buckets[0].sonnet_input_tokens as i64)
        .bind(p.buckets[0].sonnet_output_tokens as i64)
        .bind(p.buckets[0].opus_input_tokens as i64)
        .bind(p.buckets[0].opus_output_tokens as i64)
        .bind(p.buckets[1].total_input_tokens as i64)
        .bind(p.buckets[1].total_output_tokens as i64)
        .bind(p.buckets[1].sonnet_input_tokens as i64)
        .bind(p.buckets[1].sonnet_output_tokens as i64)
        .bind(p.buckets[1].opus_input_tokens as i64)
        .bind(p.buckets[1].opus_output_tokens as i64)
        .bind(p.buckets[2].total_input_tokens as i64)
        .bind(p.buckets[2].total_output_tokens as i64)
        .bind(p.buckets[2].sonnet_input_tokens as i64)
        .bind(p.buckets[2].sonnet_output_tokens as i64)
        .bind(p.buckets[2].opus_input_tokens as i64)
        .bind(p.buckets[2].opus_output_tokens as i64)
        .bind(p.buckets[3].total_input_tokens as i64)
        .bind(p.buckets[3].total_output_tokens as i64)
        .bind(p.buckets[3].sonnet_input_tokens as i64)
        .bind(p.buckets[3].sonnet_output_tokens as i64)
        .bind(p.buckets[3].opus_input_tokens as i64)
        .bind(p.buckets[3].opus_output_tokens as i64)
        .bind(p.buckets[4].total_input_tokens as i64)
        .bind(p.buckets[4].total_output_tokens as i64)
        .bind(p.buckets[4].sonnet_input_tokens as i64)
        .bind(p.buckets[4].sonnet_output_tokens as i64)
        .bind(p.buckets[4].opus_input_tokens as i64)
        .bind(p.buckets[4].opus_output_tokens as i64)
        .bind(p.session_utilization)
        .bind(p.weekly_utilization)
        .bind(p.weekly_sonnet_utilization)
        .bind(p.weekly_opus_utilization)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Update only the dispatch cooldown timestamp without clobbering usage buckets.
pub async fn set_account_reset_time(
    pool: &SqlitePool,
    account_id: i64,
    reset_time: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO account_runtime_state (account_id, reset_time, updated_at)
           VALUES (?1, ?2, CURRENT_TIMESTAMP)
           ON CONFLICT(account_id) DO UPDATE SET
             reset_time = excluded.reset_time,
             updated_at = CURRENT_TIMESTAMP"#,
    )
    .bind(account_id)
    .bind(reset_time)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark an account as disabled with a reason.
pub async fn set_account_disabled(
    pool: &SqlitePool,
    account_id: i64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE accounts SET status = 'disabled', invalid_reason = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
    )
    .bind(reason)
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_accounts_active(pool: &SqlitePool, ids: &[i64]) -> Result<(), sqlx::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "UPDATE accounts
         SET status = 'active',
             invalid_reason = NULL,
             last_error = NULL,
             updated_at = CURRENT_TIMESTAMP
         WHERE id IN ({placeholders}) AND status IN ('disabled', 'auth_error')"
    );
    let mut q = sqlx::query(&sql);
    for id in ids {
        q = q.bind(id);
    }
    q.execute(pool).await?;
    Ok(())
}

pub async fn set_account_active(pool: &SqlitePool, account_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE accounts
         SET status = 'active',
             invalid_reason = NULL,
             last_error = NULL,
             updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND status != 'disabled'",
    )
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update account telemetry metadata (email, account_type, org_uuid) only
/// if the account's current credential still matches the fingerprint the
/// caller started with. Prevents stale probes from overwriting metadata
/// after a credential rotation (cookie replacement or OAuth re-auth).
///
/// For `auth_source = "cookie"`, the guard matches against `cookie_blob`
/// (with a legacy `sessionKey=` variant for pre-normalization rows). For
/// `auth_source = "oauth"`, it matches against `oauth_access_token`.
///
/// Returns Ok(()) with a warn log if the guard misses — callers treat a
/// missed update as a no-op, not an error.
pub async fn update_account_metadata(
    pool: &SqlitePool,
    account_id: i64,
    email: &str,
    account_type: &str,
    org_uuid: &str,
    expected_auth_source: &str,
    expected_credential_prefix: &str,
) -> Result<(), sqlx::Error> {
    let result = match expected_auth_source {
        "cookie" => {
            sqlx::query(
                "UPDATE accounts SET email = ?1, account_type = ?2, organization_uuid = ?3, updated_at = CURRENT_TIMESTAMP WHERE id = ?4 AND (cookie_blob LIKE ?5 OR cookie_blob LIKE ?6)",
            )
            .bind(email)
            .bind(account_type)
            .bind(org_uuid)
            .bind(account_id)
            .bind(format!("{}%", expected_credential_prefix))
            .bind(format!("sessionKey={}%", expected_credential_prefix))
            .execute(pool)
            .await?
        }
        "oauth" => {
            sqlx::query(
                "UPDATE accounts SET email = ?1, account_type = ?2, organization_uuid = ?3, updated_at = CURRENT_TIMESTAMP WHERE id = ?4 AND oauth_access_token LIKE ?5",
            )
            .bind(email)
            .bind(account_type)
            .bind(org_uuid)
            .bind(account_id)
            .bind(format!("{}%", expected_credential_prefix))
            .execute(pool)
            .await?
        }
        other => {
            tracing::warn!(
                account_id,
                auth_source = %other,
                "update_account_metadata: unknown auth_source, skipping metadata persist"
            );
            return Ok(());
        }
    };
    if result.rows_affected() == 0 {
        tracing::warn!(
            account_id,
            auth_source = %expected_auth_source,
            "update_account_metadata guard missed: credential fingerprint changed, metadata not persisted"
        );
    }
    Ok(())
}

pub async fn update_account_metadata_unchecked(
    pool: &SqlitePool,
    account_id: i64,
    email: Option<&str>,
    account_type: Option<&str>,
    org_uuid: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE accounts
         SET email = COALESCE(?1, email),
             account_type = COALESCE(?2, account_type),
             organization_uuid = COALESCE(?3, organization_uuid),
             invalid_reason = NULL,
             updated_at = CURRENT_TIMESTAMP
         WHERE id = ?4",
    )
    .bind(email)
    .bind(account_type)
    .bind(org_uuid)
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_account_oauth(
    pool: &SqlitePool,
    account_id: i64,
    token: Option<&TokenInfo>,
    last_error: Option<&str>,
) -> Result<(), sqlx::Error> {
    let (access_token, refresh_token, expires_at, last_refresh_at) = match token {
        Some(token) => (
            Some(token.access_token.as_str()),
            Some(token.refresh_token.as_str()),
            Some(token.expires_at.to_rfc3339()),
            Some(chrono::Utc::now().to_rfc3339()),
        ),
        None => (None, None, None, None),
    };

    sqlx::query(
        "UPDATE accounts
         SET oauth_access_token = ?1,
             oauth_refresh_token = ?2,
             oauth_expires_at = ?3,
             last_refresh_at = COALESCE(?4, last_refresh_at),
             last_error = ?5,
             updated_at = CURRENT_TIMESTAMP
         WHERE id = ?6",
    )
    .bind(access_token)
    .bind(refresh_token)
    .bind(expires_at)
    .bind(last_refresh_at)
    .bind(last_error)
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_account_auth_error(
    pool: &SqlitePool,
    account_id: i64,
    last_error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE accounts
         SET status = 'auth_error',
             invalid_reason = NULL,
             last_error = ?1,
             updated_at = CURRENT_TIMESTAMP
         WHERE id = ?2",
    )
    .bind(last_error)
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_account_by_id(
    pool: &SqlitePool,
    account_id: i64,
) -> Result<Option<AccountWithRuntime>, sqlx::Error> {
    Ok(load_all_accounts(pool)
        .await?
        .into_iter()
        .find(|account| account.id == account_id))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use chrono::Utc;
    use sqlx::Row;

    use super::{
        AccountAuthSourceSummary, AccountPoolSummary, AccountStatusSummary, AccountSummary,
        AccountWithRuntime, RuntimeStateRow, summarize_accounts, update_account_metadata,
    };
    use crate::config::{Organization, TokenInfo, UsageBreakdown};
    use crate::db::init_pool;

    fn runtime(reset_time: Option<i64>) -> RuntimeStateRow {
        RuntimeStateRow {
            reset_time,
            supports_claude_1m_sonnet: None,
            supports_claude_1m_opus: None,
            count_tokens_allowed: None,
            session_resets_at: None,
            weekly_resets_at: None,
            weekly_sonnet_resets_at: None,
            weekly_opus_resets_at: None,
            resets_last_checked_at: None,
            session_has_reset: None,
            weekly_has_reset: None,
            weekly_sonnet_has_reset: None,
            weekly_opus_has_reset: None,
            session_utilization: None,
            weekly_utilization: None,
            weekly_sonnet_utilization: None,
            weekly_opus_utilization: None,
            buckets: std::array::from_fn(|_| UsageBreakdown::default()),
        }
    }

    fn account(
        id: i64,
        auth_source: &str,
        status: &str,
        cookie_blob: Option<&str>,
        has_oauth: bool,
        reset_time: Option<i64>,
    ) -> AccountWithRuntime {
        AccountWithRuntime {
            id,
            name: format!("acct-{id}"),
            rr_order: id,
            max_slots: 5,
            proxy_id: None,
            proxy_name: None,
            proxy_url: None,
            drain_first: false,
            status: status.to_string(),
            auth_source: auth_source.to_string(),
            cookie_blob: cookie_blob.map(str::to_string),
            oauth_token: has_oauth.then(|| TokenInfo {
                access_token: "access".to_string(),
                expires_in: Duration::from_secs(3600),
                organization: Organization {
                    uuid: "org".to_string(),
                },
                refresh_token: "refresh".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
            }),
            oauth_expires_at: None,
            last_refresh_at: None,
            last_error: None,
            organization_uuid: Some("org".to_string()),
            invalid_reason: None,
            email: None,
            account_type: None,
            created_at: None,
            updated_at: None,
            runtime: Some(runtime(reset_time)),
        }
    }

    #[test]
    fn summarize_accounts_unifies_pool_and_status_views() {
        let now = Utc::now().timestamp();
        let summary = summarize_accounts(&[
            account(1, "oauth", "active", None, true, None),
            account(
                2,
                "cookie",
                "active",
                Some("cookie=yes"),
                false,
                Some(now + 300),
            ),
            account(3, "cookie", "auth_error", Some("cookie=yes"), false, None),
            account(4, "oauth", "disabled", None, true, None),
            account(5, "oauth", "active", None, false, None),
        ]);

        assert_eq!(
            summary,
            AccountSummary {
                total: 5,
                pool: AccountPoolSummary {
                    valid: 1,
                    exhausted: 1,
                    invalid: 2,
                },
                statuses: AccountStatusSummary {
                    active: 2,
                    cooling: 1,
                    error: 1,
                    disabled: 1,
                },
                auth_sources: AccountAuthSourceSummary {
                    oauth: 3,
                    cookie: 2,
                },
            }
        );
    }

    #[tokio::test]
    async fn update_account_metadata_allows_normal_and_legacy_prefix_only() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source, cookie_blob, drain_first
            ) VALUES
                (1, 'normal', 1, 5, 'active', 'cookie', ?1, 0),
                (2, 'legacy', 2, 5, 'active', 'cookie', ?2, 0),
                (3, 'embedded', 3, 5, 'active', 'cookie', ?3, 0)",
        )
        .bind("sk-ant-sid02-ABCDEFG-rest")
        .bind("sessionKey=sk-ant-sid02-ABCDEFG-rest")
        .bind("xxsk-ant-sid02-ABCDEFG-rest")
        .execute(&pool)
        .await
        .unwrap();

        let prefix = "sk-ant-sid02-ABCDEFG";
        update_account_metadata(
            &pool,
            1,
            "n@example.com",
            "pro",
            "org-normal",
            "cookie",
            prefix,
        )
        .await
        .unwrap();
        update_account_metadata(
            &pool,
            2,
            "l@example.com",
            "pro",
            "org-legacy",
            "cookie",
            prefix,
        )
        .await
        .unwrap();
        update_account_metadata(
            &pool,
            3,
            "e@example.com",
            "pro",
            "org-embedded",
            "cookie",
            prefix,
        )
        .await
        .unwrap();

        let normal =
            sqlx::query("SELECT email, account_type, organization_uuid FROM accounts WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            normal.get::<Option<String>, _>("email").as_deref(),
            Some("n@example.com")
        );
        assert_eq!(
            normal.get::<Option<String>, _>("account_type").as_deref(),
            Some("pro")
        );
        assert_eq!(
            normal
                .get::<Option<String>, _>("organization_uuid")
                .as_deref(),
            Some("org-normal")
        );

        let legacy =
            sqlx::query("SELECT email, account_type, organization_uuid FROM accounts WHERE id = 2")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            legacy.get::<Option<String>, _>("email").as_deref(),
            Some("l@example.com")
        );
        assert_eq!(
            legacy.get::<Option<String>, _>("account_type").as_deref(),
            Some("pro")
        );
        assert_eq!(
            legacy
                .get::<Option<String>, _>("organization_uuid")
                .as_deref(),
            Some("org-legacy")
        );

        let embedded =
            sqlx::query("SELECT email, account_type, organization_uuid FROM accounts WHERE id = 3")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(embedded.get::<Option<String>, _>("email"), None);
        assert_eq!(embedded.get::<Option<String>, _>("account_type"), None);
        assert_eq!(embedded.get::<Option<String>, _>("organization_uuid"), None);
    }

    #[tokio::test]
    async fn update_account_metadata_oauth_branch_matches_access_token_prefix() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source,
                oauth_access_token, oauth_refresh_token, oauth_expires_at,
                organization_uuid, drain_first
            ) VALUES (1, 'oa', 1, 5, 'active', 'oauth', ?1, ?2, '2030-01-01T00:00:00Z', 'seed', 0)",
        )
        .bind("at-abc123xyz-secret")
        .bind("rt-refresh")
        .execute(&pool)
        .await
        .unwrap();

        // Matching access_token prefix → UPDATE lands.
        update_account_metadata(
            &pool,
            1,
            "o@example.com",
            "max",
            "org-fresh",
            "oauth",
            "at-abc123",
        )
        .await
        .unwrap();

        let row =
            sqlx::query("SELECT email, account_type, organization_uuid FROM accounts WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            row.get::<Option<String>, _>("email").as_deref(),
            Some("o@example.com")
        );
        assert_eq!(
            row.get::<Option<String>, _>("organization_uuid").as_deref(),
            Some("org-fresh")
        );
    }

    #[tokio::test]
    async fn update_account_metadata_oauth_guard_misses_after_access_token_rotation() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source,
                oauth_access_token, oauth_refresh_token, oauth_expires_at,
                organization_uuid, drain_first
            ) VALUES (2, 'oa', 1, 5, 'active', 'oauth', ?1, ?2, '2030-01-01T00:00:00Z', 'seed', 0)",
        )
        .bind("at-new-rotated-token")
        .bind("rt-refresh")
        .execute(&pool)
        .await
        .unwrap();

        // Probe started with old prefix, access_token was rotated mid-flight.
        update_account_metadata(
            &pool,
            2,
            "stale@example.com",
            "max",
            "stale-org",
            "oauth",
            "at-old-prefix",
        )
        .await
        .unwrap();

        let row = sqlx::query("SELECT email, organization_uuid FROM accounts WHERE id = 2")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<Option<String>, _>("email"), None);
        assert_eq!(
            row.get::<Option<String>, _>("organization_uuid").as_deref(),
            Some("seed"),
            "rotated access_token must block stale probe's metadata write"
        );
    }

    #[tokio::test]
    async fn update_account_metadata_skips_on_auth_source_mismatch() {
        // Cookie account probed as "oauth" (or vice versa) must no-op,
        // not silently write against a different credential column.
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source, cookie_blob, drain_first
            ) VALUES (3, 'ck', 1, 5, 'active', 'cookie', ?1, 0)",
        )
        .bind("sk-ant-sid02-ABCDEFG-rest")
        .execute(&pool)
        .await
        .unwrap();

        update_account_metadata(
            &pool,
            3,
            "should@not.write",
            "pro",
            "should-not-write",
            "oauth",
            "sk-ant-sid02",
        )
        .await
        .unwrap();

        let row = sqlx::query("SELECT email, organization_uuid FROM accounts WHERE id = 3")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<Option<String>, _>("email"), None);
        assert_eq!(
            row.get::<Option<String>, _>("organization_uuid"),
            None,
            "oauth guard on cookie account must miss — schema has no oauth_access_token"
        );
    }

    #[tokio::test]
    async fn accounts_check_rejects_legacy_hybrid_auth_source() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        let result = sqlx::query(
            "INSERT INTO accounts (
                name, rr_order, max_slots, status, auth_source, cookie_blob, drain_first
            ) VALUES ('h', 1, 5, 'active', 'hybrid', 'ck', 0)",
        )
        .execute(&pool)
        .await;
        assert!(
            result.is_err(),
            "auth_source='hybrid' must be rejected by CHECK",
        );
    }

    #[tokio::test]
    async fn accounts_check_rejects_cookie_row_with_oauth_tokens() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        let result = sqlx::query(
            "INSERT INTO accounts (
                name, rr_order, max_slots, status, auth_source,
                cookie_blob, oauth_access_token, oauth_refresh_token, oauth_expires_at,
                drain_first
            ) VALUES ('dual', 1, 5, 'active', 'cookie', 'ck', 'at', 'rt', '2030-01-01T00:00:00Z', 0)",
        )
        .execute(&pool)
        .await;
        assert!(
            result.is_err(),
            "cookie row with oauth tokens must be rejected by mutex CHECK",
        );
    }

    #[tokio::test]
    async fn accounts_check_rejects_cookie_auth_without_cookie_blob() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        let result = sqlx::query(
            "INSERT INTO accounts (
                name, rr_order, max_slots, status, auth_source, drain_first
            ) VALUES ('c', 1, 5, 'active', 'cookie', 0)",
        )
        .execute(&pool)
        .await;
        assert!(
            result.is_err(),
            "cookie auth without cookie_blob must be rejected by mutex CHECK",
        );
    }

    #[tokio::test]
    async fn accounts_check_rejects_oauth_auth_without_tokens() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        let result = sqlx::query(
            "INSERT INTO accounts (
                name, rr_order, max_slots, status, auth_source, drain_first
            ) VALUES ('o', 1, 5, 'active', 'oauth', 0)",
        )
        .execute(&pool)
        .await;
        assert!(
            result.is_err(),
            "oauth auth without tokens must be rejected by mutex CHECK",
        );
    }

    #[tokio::test]
    async fn accounts_check_accepts_valid_cookie_and_oauth_rows() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        sqlx::query(
            "INSERT INTO accounts (
                name, rr_order, max_slots, status, auth_source, cookie_blob, drain_first
            ) VALUES ('c', 1, 5, 'active', 'cookie', 'ck', 0)",
        )
        .execute(&pool)
        .await
        .expect("cookie-only row should satisfy mutex CHECK");

        sqlx::query(
            "INSERT INTO accounts (
                name, rr_order, max_slots, status, auth_source,
                oauth_access_token, oauth_refresh_token, oauth_expires_at, drain_first
            ) VALUES ('o', 2, 5, 'active', 'oauth', 'at', 'rt', '2030-01-01T00:00:00Z', 0)",
        )
        .execute(&pool)
        .await
        .expect("oauth-only row should satisfy mutex CHECK");
    }

    async fn insert_oauth_account(pool: &sqlx::SqlitePool, id: i64, name: &str, rr: i64) {
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source,
                oauth_access_token, oauth_refresh_token, oauth_expires_at, drain_first
            ) VALUES (?1, ?2, ?3, 5, 'active', 'oauth', 'old-at', 'old-rt', '2030-01-01T00:00:00Z', 0)",
        )
        .bind(id)
        .bind(name)
        .bind(rr)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_cookie_account(pool: &sqlx::SqlitePool, id: i64, name: &str, rr: i64) {
        sqlx::query(
            "INSERT INTO accounts (
                id, name, rr_order, max_slots, status, auth_source, cookie_blob, drain_first
            ) VALUES (?1, ?2, ?3, 5, 'active', 'cookie', 'old-ck', 0)",
        )
        .bind(id)
        .bind(name)
        .bind(rr)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Mirrors the consolidated single-statement oauth-row-to-cookie replacement
    /// run by `src/api/admin/accounts.rs::update`. Any regression that splits
    /// this into multiple UPDATEs will fail the mutex CHECK mid-transaction.
    #[tokio::test]
    async fn update_sql_replaces_oauth_account_with_cookie_atomically() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        insert_oauth_account(&pool, 1, "a", 1).await;

        sqlx::query(
            "UPDATE accounts
             SET cookie_blob = ?1,
                 oauth_access_token = NULL,
                 oauth_refresh_token = NULL,
                 oauth_expires_at = NULL,
                 last_refresh_at = NULL,
                 auth_source = 'cookie',
                 invalid_reason = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2",
        )
        .bind("new-ck")
        .bind(1_i64)
        .execute(&pool)
        .await
        .expect("consolidated oauth->cookie replacement must satisfy CHECK");

        let row = sqlx::query(
            "SELECT auth_source, cookie_blob, oauth_access_token FROM accounts WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("auth_source"), "cookie");
        assert_eq!(
            row.get::<Option<String>, _>("cookie_blob").as_deref(),
            Some("new-ck"),
        );
        assert_eq!(row.get::<Option<String>, _>("oauth_access_token"), None);
    }

    #[tokio::test]
    async fn update_sql_replaces_cookie_account_with_oauth_atomically() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        insert_cookie_account(&pool, 1, "a", 1).await;

        sqlx::query(
            "UPDATE accounts
             SET cookie_blob = NULL,
                 oauth_access_token = ?1,
                 oauth_refresh_token = ?2,
                 oauth_expires_at = ?3,
                 last_refresh_at = ?4,
                 organization_uuid = ?5,
                 auth_source = 'oauth',
                 last_error = NULL,
                 invalid_reason = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?6",
        )
        .bind("new-at")
        .bind("new-rt")
        .bind("2031-01-01T00:00:00Z")
        .bind("2026-04-21T00:00:00Z")
        .bind("org-new")
        .bind(1_i64)
        .execute(&pool)
        .await
        .expect("consolidated cookie->oauth replacement must satisfy CHECK");

        let row = sqlx::query(
            "SELECT auth_source, cookie_blob, oauth_access_token, organization_uuid
             FROM accounts WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("auth_source"), "oauth");
        assert_eq!(row.get::<Option<String>, _>("cookie_blob"), None);
        assert_eq!(
            row.get::<Option<String>, _>("oauth_access_token")
                .as_deref(),
            Some("new-at"),
        );
        assert_eq!(
            row.get::<Option<String>, _>("organization_uuid").as_deref(),
            Some("org-new"),
        );
    }

    /// Regression guard: piecewise credential writes — the pre-fix shape of
    /// update() — trip the mutex CHECK when the DB still disagrees with the
    /// column being changed. Documents why the replacement SQL must stay
    /// consolidated in a single UPDATE.
    #[tokio::test]
    async fn update_sql_piecewise_cookie_write_on_oauth_row_trips_check() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        insert_oauth_account(&pool, 1, "a", 1).await;

        let result = sqlx::query(
            "UPDATE accounts SET cookie_blob = 'new-ck', updated_at = CURRENT_TIMESTAMP WHERE id = 1",
        )
        .execute(&pool)
        .await;
        assert!(
            result.is_err(),
            "writing cookie_blob onto an oauth row without also switching auth_source must fail CHECK",
        );
    }

    #[tokio::test]
    async fn update_sql_piecewise_cookie_clear_on_cookie_row_trips_check() {
        let pool = init_pool(Path::new(":memory:")).await.unwrap();
        insert_cookie_account(&pool, 1, "a", 1).await;

        let result = sqlx::query(
            "UPDATE accounts SET cookie_blob = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = 1",
        )
        .execute(&pool)
        .await;
        assert!(
            result.is_err(),
            "clearing cookie_blob on a cookie row without switching auth_source must fail CHECK",
        );
    }

    /// Regression guard for the reviewer-flagged drift shapes that tripped
    /// the C3 migration. Builds a pre-C3 accounts schema, seeds rows that
    /// can only exist before the mutex CHECK lands, runs the real migration
    /// file end to end, and asserts every survivor sits in the canonical
    /// cookie / oauth shape that the new CHECK expects.
    #[tokio::test]
    async fn migration_canonicalizes_partial_credential_drift() {
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();

        sqlx::query("CREATE TABLE proxies (id INTEGER PRIMARY KEY, name TEXT UNIQUE)")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE accounts (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                rr_order INTEGER NOT NULL UNIQUE,
                max_slots INTEGER NOT NULL DEFAULT 5,
                status TEXT NOT NULL DEFAULT 'active',
                auth_source TEXT NOT NULL CHECK (auth_source IN ('cookie', 'oauth', 'hybrid')),
                cookie_blob BLOB,
                oauth_access_token BLOB,
                oauth_refresh_token BLOB,
                oauth_expires_at TEXT,
                organization_uuid TEXT,
                last_refresh_at TEXT,
                last_used_at TEXT,
                last_error TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                invalid_reason TEXT,
                email TEXT,
                account_type TEXT,
                drain_first INTEGER NOT NULL DEFAULT 0,
                proxy_id INTEGER REFERENCES proxies(id) ON DELETE SET NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Seed every drift shape that could exist before C3:
        //   1. reviewer case 1: cookie row with a stray oauth_refresh_token
        //   2. reviewer case 2: oauth row with only access_token, no cookie → unrecoverable
        //   3. oauth-labeled row missing expires_at but with a cookie_blob → salvage as cookie
        //   4. cookie-labeled row carrying a full oauth set → salvage as oauth
        //   5. normal cookie row (control)
        //   6. normal oauth row (control)
        //   7. legacy hybrid row with full oauth + cookie → oauth wins
        //   8. legacy hybrid row with only cookie → falls back to cookie
        sqlx::query(
            "INSERT INTO accounts (name, rr_order, auth_source, cookie_blob, oauth_refresh_token)
             VALUES ('case1_cookie_with_residual_rt', 1, 'cookie', 'ck1', 'residual-rt')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO accounts (name, rr_order, auth_source, oauth_access_token)
             VALUES ('case2_oauth_incomplete_no_cookie', 2, 'oauth', 'at2')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO accounts (name, rr_order, auth_source, cookie_blob, oauth_access_token, oauth_refresh_token)
             VALUES ('case3_oauth_incomplete_with_cookie', 3, 'oauth', 'ck3', 'at3', 'rt3')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO accounts (name, rr_order, auth_source, cookie_blob, oauth_access_token, oauth_refresh_token, oauth_expires_at)
             VALUES ('case4_cookie_with_full_oauth', 4, 'cookie', 'ck4', 'at4', 'rt4', '2030-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO accounts (name, rr_order, auth_source, cookie_blob)
             VALUES ('case5_normal_cookie', 5, 'cookie', 'ck5')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO accounts (name, rr_order, auth_source, oauth_access_token, oauth_refresh_token, oauth_expires_at)
             VALUES ('case6_normal_oauth', 6, 'oauth', 'at6', 'rt6', '2030-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO accounts (name, rr_order, auth_source, cookie_blob, oauth_access_token, oauth_refresh_token, oauth_expires_at)
             VALUES ('case7_legacy_hybrid_both', 7, 'hybrid', 'ck7', 'at7', 'rt7', '2030-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO accounts (name, rr_order, auth_source, cookie_blob)
             VALUES ('case8_legacy_hybrid_cookie_only', 8, 'hybrid', 'ck8')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Apply the real C3 migration — splitting on ';' is safe because the
        // file has no statement-internal semicolons (CHECK bodies only hold
        // column references and boolean ops).
        let migration_sql =
            include_str!("../../migrations/20260421000003_drop_hybrid_auth_source.sql");
        for statement in migration_sql.split(';') {
            let trimmed = statement.trim();
            if trimmed.is_empty() {
                continue;
            }
            sqlx::query(trimmed)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("migration statement failed: {e}\n---\n{trimmed}\n"));
        }

        type SurvivorRow = (
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let survivors: Vec<SurvivorRow> = sqlx::query_as(
            "SELECT id, name, auth_source, cookie_blob, oauth_access_token, oauth_refresh_token, oauth_expires_at
                 FROM accounts ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        // case 2 is the only unrecoverable row — it should be gone.
        let names: Vec<&str> = survivors.iter().map(|row| row.1.as_str()).collect();
        assert!(
            !names.contains(&"case2_oauth_incomplete_no_cookie"),
            "unrecoverable row (no cookie + incomplete oauth) must be deleted",
        );

        // Every survivor must match the canonical cookie-or-oauth shape
        // that the new CHECK enforces.
        for (id, name, auth_source, cookie_blob, at, rt, expires) in &survivors {
            match auth_source.as_str() {
                "cookie" => {
                    assert!(
                        cookie_blob.is_some(),
                        "{name}: cookie row must have cookie_blob"
                    );
                    assert!(
                        at.is_none() && rt.is_none() && expires.is_none(),
                        "{name}: cookie row must have all oauth_* NULL"
                    );
                }
                "oauth" => {
                    assert!(
                        cookie_blob.is_none(),
                        "{name}: oauth row must have cookie_blob NULL"
                    );
                    assert!(
                        at.is_some() && rt.is_some() && expires.is_some(),
                        "{name}: oauth row must have full oauth token set"
                    );
                }
                other => panic!("{name} (id={id}): unexpected auth_source {other:?}"),
            }
        }

        // Spot-check the salvage decisions case by case.
        let by_name: std::collections::HashMap<&str, &str> = survivors
            .iter()
            .map(|row| (row.1.as_str(), row.2.as_str()))
            .collect();
        assert_eq!(
            by_name.get("case1_cookie_with_residual_rt"),
            Some(&"cookie")
        );
        assert_eq!(
            by_name.get("case3_oauth_incomplete_with_cookie"),
            Some(&"cookie"),
            "incomplete oauth with cookie should salvage as cookie"
        );
        assert_eq!(
            by_name.get("case4_cookie_with_full_oauth"),
            Some(&"oauth"),
            "complete oauth wins over stale cookie label"
        );
        assert_eq!(by_name.get("case5_normal_cookie"), Some(&"cookie"));
        assert_eq!(by_name.get("case6_normal_oauth"), Some(&"oauth"));
        assert_eq!(
            by_name.get("case7_legacy_hybrid_both"),
            Some(&"oauth"),
            "hybrid with full oauth should normalize to oauth"
        );
        assert_eq!(
            by_name.get("case8_legacy_hybrid_cookie_only"),
            Some(&"cookie"),
            "hybrid with only cookie should normalize to cookie"
        );
    }
}
