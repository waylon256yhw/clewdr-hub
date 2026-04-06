use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::SqlitePool;
use tracing::warn;

use crate::db::billing::get_setting;

/// Default values (compile-time fallbacks)
pub const DEFAULT_CLI_VERSION: &str = "2.1.80";
pub const DEFAULT_BILLING_SALT: &str = "59cf53e54c78";

/// Cached stealth configuration loaded from DB settings.
#[derive(Clone, Debug)]
pub struct StealthProfile {
    pub cli_version: String,
    pub billing_salt: String,
    pub proxy: Option<String>,
}

impl Default for StealthProfile {
    fn default() -> Self {
        Self {
            cli_version: DEFAULT_CLI_VERSION.into(),
            billing_salt: DEFAULT_BILLING_SALT.into(),
            proxy: None,
        }
    }
}

impl StealthProfile {
    /// Load profile from DB settings, falling back to defaults for missing keys.
    pub async fn load_from_db(pool: &SqlitePool) -> Self {
        let mut profile = Self::default();

        fn non_empty(v: Result<Option<String>, sqlx::Error>) -> Option<String> {
            v.ok().flatten().filter(|s| !s.is_empty())
        }

        if let Some(v) = non_empty(get_setting(pool, "cc_cli_version").await) {
            profile.cli_version = v;
        }
        if let Some(v) = non_empty(get_setting(pool, "cc_billing_salt").await) {
            profile.billing_salt = v;
        }
        profile.proxy = non_empty(get_setting(pool, "proxy").await);

        profile
    }

    /// User-Agent string: `claude-cli/{version} (external, cli)`
    pub fn user_agent(&self) -> String {
        format!("claude-cli/{} (external, cli)", self.cli_version)
    }
}

/// Global stealth profile, loaded once at startup and swappable at runtime.
pub type SharedStealthProfile = Arc<ArcSwap<StealthProfile>>;

/// Global singleton, initialized at startup via `init_stealth_profile()`.
static GLOBAL_PROFILE: std::sync::OnceLock<SharedStealthProfile> = std::sync::OnceLock::new();

/// Get the global stealth profile. Panics if not initialized.
pub fn global_profile() -> &'static SharedStealthProfile {
    GLOBAL_PROFILE
        .get()
        .expect("stealth profile not initialized")
}

/// Create and register the global shared profile from DB.
pub async fn init_stealth_profile(pool: &SqlitePool) -> SharedStealthProfile {
    let profile = StealthProfile::load_from_db(pool).await;
    warn!("Stealth profile loaded: cli={}", profile.cli_version);
    let shared = Arc::new(ArcSwap::from_pointee(profile));
    let _ = GLOBAL_PROFILE.set(shared.clone());
    shared
}

/// Reload stealth profile from DB and hot-swap into global singleton.
pub async fn reload_stealth_profile(pool: &SqlitePool) {
    let profile = StealthProfile::load_from_db(pool).await;
    warn!("Stealth profile reloaded: cli={}", profile.cli_version);
    global_profile().store(Arc::new(profile));
}
