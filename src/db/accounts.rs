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
    pub hybrid: i64,
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
            "hybrid" => summary.auth_sources.hybrid += 1,
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

/// Update account telemetry metadata (email, account_type, org_uuid).
/// Only updates if the account's cookie_blob still matches the expected value,
/// preventing stale probes from overwriting metadata after cookie replacement.
pub async fn update_account_metadata(
    pool: &SqlitePool,
    account_id: i64,
    email: &str,
    account_type: &str,
    org_uuid: &str,
    expected_cookie_prefix: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE accounts SET email = ?1, account_type = ?2, organization_uuid = ?3, updated_at = CURRENT_TIMESTAMP WHERE id = ?4 AND cookie_blob LIKE ?5",
    )
    .bind(email)
    .bind(account_type)
    .bind(org_uuid)
    .bind(account_id)
    .bind(format!("{}%", expected_cookie_prefix))
    .execute(pool)
    .await?;
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

pub async fn load_pure_oauth_accounts(
    pool: &SqlitePool,
    bound_ids: &[i64],
) -> Result<Vec<AccountWithRuntime>, sqlx::Error> {
    let all = load_all_accounts(pool).await?;
    Ok(all
        .into_iter()
        .filter(|account| {
            !matches!(account.status.as_str(), "auth_error" | "disabled")
                && !is_temporarily_unavailable(account)
                && account.auth_source == "oauth"
                && account.oauth_token.is_some()
                && (bound_ids.is_empty() || bound_ids.contains(&account.id))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;

    use super::{
        AccountAuthSourceSummary, AccountPoolSummary, AccountStatusSummary, AccountSummary,
        AccountWithRuntime, RuntimeStateRow, summarize_accounts,
    };
    use crate::config::{Organization, TokenInfo, UsageBreakdown};

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
            account(3, "hybrid", "auth_error", Some("cookie=yes"), true, None),
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
                    cookie: 1,
                    hybrid: 1,
                },
            }
        );
    }
}
