#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub password_hash: Option<String>,
    pub role: String,
    pub policy_id: i64,
    pub disabled_at: Option<String>,
    pub last_seen_at: Option<String>,
    pub notes: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKey {
    pub id: i64,
    pub user_id: i64,
    pub label: Option<String>,
    pub lookup_key: String,
    pub key_hash: Vec<u8>,
    pub disabled_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub last_used_ip: Option<String>,
    pub created_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Policy {
    pub id: i64,
    pub name: String,
    pub max_concurrent: i64,
    pub rpm_limit: i64,
    pub weekly_budget_nanousd: i64,
    pub monthly_budget_nanousd: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub user_id: i64,
    pub username: String,
    pub role: String,
    pub api_key_id: Option<i64>,
    pub policy_id: i64,
    pub max_concurrent: i32,
    pub rpm_limit: i32,
    pub weekly_budget_nanousd: i64,
    pub monthly_budget_nanousd: i64,
    pub bound_account_ids: Vec<i64>,
    pub auto_cache_enabled: bool,
    pub enhanced_audit_enabled: bool,
}

/// Snapshot of the per-request audit metadata, populated at auth time
/// **only when the API key has `enhanced_audit_enabled = true`**. Plumbed
/// through `ClaudeContext` / `BillingContext` and persisted into
/// `request_log_audits` inside the terminal-log transaction.
///
/// Existence of the snapshot is the authoritative "this request was
/// audited" signal — auth puts `None` here for non-audited keys and for
/// admin cookie sessions, and the billing layer skips the sidecar write
/// in those cases.
///
/// White-listed fields only: no Authorization, Cookie, request body,
/// prompts, or tool arguments. All caller-controlled strings are
/// truncated at the auth boundary (see `auth.rs`) to keep
/// `request_log_audits` rows bounded even for an audited but adversarial
/// key.
#[derive(Debug, Clone)]
pub struct RequestAuditSnapshot {
    pub peer_ip: String,
    pub client_ip: String,
    pub ip_source: &'static str,
    pub forwarded_chain: Option<String>,
    pub user_agent: Option<String>,
    pub anthropic_version: Option<String>,
    pub anthropic_beta: Option<String>,
    /// Filled by the per-surface preprocess after auth — `None` here
    /// means the request didn't reach a known API entry. Stored as
    /// `Option<&'static str>` so misuse is a compile error.
    pub api_surface: Option<&'static str>,
    pub content_length: Option<i64>,
}
