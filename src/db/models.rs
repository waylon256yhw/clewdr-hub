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
    pub api_key_id: i64,
    pub policy_id: i64,
    pub max_concurrent: i32,
    pub rpm_limit: i32,
    pub weekly_budget_nanousd: i64,
    pub monthly_budget_nanousd: i64,
}
