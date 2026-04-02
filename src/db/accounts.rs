use sqlx::{Row, SqlitePool};

use crate::config::{RuntimeStateParams, UsageBreakdown};

/// Joined result of accounts + account_runtime_state.
#[derive(Debug)]
pub struct AccountWithRuntime {
    pub id: i64,
    pub name: String,
    pub rr_order: i64,
    pub max_slots: i64,
    pub status: String,
    pub cookie_blob: String,
    pub invalid_reason: Option<String>,
    pub email: Option<String>,
    pub account_type: Option<String>,
    pub runtime: Option<RuntimeStateRow>,
}

#[derive(Debug)]
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
            a.id, a.name, a.rr_order, a.max_slots, a.status, a.cookie_blob, a.invalid_reason,
            a.email, a.account_type,
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

        result.push(AccountWithRuntime {
            id: row.get("id"),
            name: row.get("name"),
            rr_order: row.get("rr_order"),
            max_slots: row.get("max_slots"),
            status: row.get("status"),
            cookie_blob: row.get("cookie_blob"),
            invalid_reason: row.get("invalid_reason"),
            email: row.get("email"),
            account_type: row.get("account_type"),
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
