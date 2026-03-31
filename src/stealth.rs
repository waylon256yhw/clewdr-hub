use std::sync::Arc;

use arc_swap::ArcSwap;
use http::{HeaderMap, HeaderName, HeaderValue};
use sqlx::SqlitePool;
use tracing::warn;
use uuid::Uuid;

use crate::db::billing::get_setting;

/// Default values (compile-time fallbacks)
pub const DEFAULT_CLI_VERSION: &str = "2.1.80";
pub const DEFAULT_SDK_VERSION: &str = "0.74.0";
pub const DEFAULT_NODE_VERSION: &str = "v24.3.0";
pub const DEFAULT_STAINLESS_OS: &str = "Linux";
pub const DEFAULT_STAINLESS_ARCH: &str = "x64";
pub const DEFAULT_BILLING_SALT: &str = "59cf53e54c78";
pub const DEFAULT_BETA_FLAGS: &str = "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advanced-tool-use-2025-11-20,effort-2025-11-24";

const CONTEXT_1M_TOKEN: &str = "context-1m-2025-08-07";

/// Endpoint type determines which headers to send.
pub enum EndpointKind {
    /// /v1/messages, /v1/messages/count_tokens — full CLI fingerprint
    DirectApi { use_context_1m: bool },
    /// /api/oauth/usage — similar to DirectApi but GET
    UsageApi,
    /// /api/bootstrap — browser-like, needs Origin/Referer/Cookie
    Console,
}

/// Cached stealth configuration loaded from DB settings.
#[derive(Clone, Debug)]
pub struct StealthProfile {
    pub cli_version: String,
    pub sdk_version: String,
    pub beta_flags: String,
    pub billing_salt: String,
    pub stainless_os: String,
    pub stainless_arch: String,
    pub node_version: String,
}

impl Default for StealthProfile {
    fn default() -> Self {
        Self {
            cli_version: DEFAULT_CLI_VERSION.into(),
            sdk_version: DEFAULT_SDK_VERSION.into(),
            beta_flags: DEFAULT_BETA_FLAGS.into(),
            billing_salt: DEFAULT_BILLING_SALT.into(),
            stainless_os: DEFAULT_STAINLESS_OS.into(),
            stainless_arch: DEFAULT_STAINLESS_ARCH.into(),
            node_version: DEFAULT_NODE_VERSION.into(),
        }
    }
}

impl StealthProfile {
    /// Load profile from DB settings, falling back to defaults for missing keys.
    pub async fn load_from_db(pool: &SqlitePool) -> Self {
        let mut profile = Self::default();

        async fn read(pool: &SqlitePool, key: &str) -> Option<String> {
            get_setting(pool, key).await.ok().flatten().filter(|v| !v.is_empty())
        }

        if let Some(v) = read(pool, "cc_cli_version").await {
            profile.cli_version = v;
        }
        if let Some(v) = read(pool, "cc_sdk_version").await {
            profile.sdk_version = v;
        }
        if let Some(v) = read(pool, "cc_node_version").await {
            profile.node_version = v;
        }
        if let Some(v) = read(pool, "cc_stainless_os").await {
            profile.stainless_os = v;
        }
        if let Some(v) = read(pool, "cc_stainless_arch").await {
            profile.stainless_arch = v;
        }
        if let Some(v) = read(pool, "cc_beta_flags").await {
            profile.beta_flags = v;
        }
        if let Some(v) = read(pool, "cc_billing_salt").await {
            profile.billing_salt = v;
        }

        profile
    }

    /// User-Agent string: `claude-cli/{version} (external, cli)`
    pub fn user_agent(&self) -> String {
        format!("claude-cli/{} (external, cli)", self.cli_version)
    }

    /// Build beta flags string, optionally removing context-1m.
    pub fn beta_flags_for(&self, use_context_1m: bool) -> String {
        if use_context_1m {
            self.beta_flags.clone()
        } else {
            self.beta_flags
                .split(',')
                .filter(|t| t.trim() != CONTEXT_1M_TOKEN)
                .collect::<Vec<_>>()
                .join(",")
        }
    }
}

/// Build the complete header set for a given endpoint kind.
pub fn build_stealth_headers(profile: &StealthProfile, kind: EndpointKind) -> HeaderMap {
    let mut headers = HeaderMap::new();

    match kind {
        EndpointKind::DirectApi { use_context_1m } => {
            insert_cli_headers(&mut headers, profile);
            let beta = profile.beta_flags_for(use_context_1m);
            headers.insert(
                HeaderName::from_static("anthropic-beta"),
                HeaderValue::from_str(&beta).unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        }
        EndpointKind::UsageApi => {
            insert_cli_headers(&mut headers, profile);
            // Usage API only needs oauth beta
            headers.insert(
                HeaderName::from_static("anthropic-beta"),
                HeaderValue::from_static("oauth-2025-04-20"),
            );
        }
        EndpointKind::Console => {
            // Console endpoints keep browser-like headers — handled by build_request()
            // Only set UA here
            if let Ok(ua) = HeaderValue::from_str(&profile.user_agent()) {
                headers.insert(http::header::USER_AGENT, ua);
            }
            return headers;
        }
    }

    headers
}

/// Insert all CLI-persona headers (shared between DirectApi and UsageApi).
fn insert_cli_headers(headers: &mut HeaderMap, profile: &StealthProfile) {
    if let Ok(ua) = HeaderValue::from_str(&profile.user_agent()) {
        headers.insert(http::header::USER_AGENT, ua);
    }
    headers.insert(
        HeaderName::from_static("x-app"),
        HeaderValue::from_static("cli"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-dangerous-direct-browser-access"),
        HeaderValue::from_static("true"),
    );

    // Session ID (random per request, mimics CC CLI session tracking)
    headers.insert(
        HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_str(&Uuid::new_v4().to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );

    // Stainless SDK fingerprint
    headers.insert(
        HeaderName::from_static("x-stainless-lang"),
        HeaderValue::from_static("js"),
    );
    if let Ok(v) = HeaderValue::from_str(&profile.sdk_version) {
        headers.insert(HeaderName::from_static("x-stainless-package-version"), v);
    }
    if let Ok(v) = HeaderValue::from_str(&profile.stainless_os) {
        headers.insert(HeaderName::from_static("x-stainless-os"), v);
    }
    if let Ok(v) = HeaderValue::from_str(&profile.stainless_arch) {
        headers.insert(HeaderName::from_static("x-stainless-arch"), v);
    }
    headers.insert(
        HeaderName::from_static("x-stainless-runtime"),
        HeaderValue::from_static("node"),
    );
    if let Ok(v) = HeaderValue::from_str(&profile.node_version) {
        headers.insert(HeaderName::from_static("x-stainless-runtime-version"), v);
    }
    headers.insert(
        HeaderName::from_static("x-stainless-retry-count"),
        HeaderValue::from_static("0"),
    );
    headers.insert(
        HeaderName::from_static("x-stainless-timeout"),
        HeaderValue::from_static("600"),
    );
}

/// Global stealth profile, loaded once at startup and swappable at runtime.
pub type SharedStealthProfile = Arc<ArcSwap<StealthProfile>>;

/// Global singleton, initialized at startup via `init_stealth_profile()`.
static GLOBAL_PROFILE: std::sync::OnceLock<SharedStealthProfile> = std::sync::OnceLock::new();

/// Get the global stealth profile. Panics if not initialized.
pub fn global_profile() -> &'static SharedStealthProfile {
    GLOBAL_PROFILE.get().expect("stealth profile not initialized")
}

/// Create and register the global shared profile from DB.
pub async fn init_stealth_profile(pool: &SqlitePool) -> SharedStealthProfile {
    let profile = StealthProfile::load_from_db(pool).await;
    warn!(
        "Stealth profile loaded: cli={} sdk={} os={}/{}",
        profile.cli_version, profile.sdk_version, profile.stainless_os, profile.stainless_arch
    );
    let shared = Arc::new(ArcSwap::from_pointee(profile));
    let _ = GLOBAL_PROFILE.set(shared.clone());
    shared
}

/// Reload stealth profile from DB and hot-swap into global singleton.
pub async fn reload_stealth_profile(pool: &SqlitePool) {
    let profile = StealthProfile::load_from_db(pool).await;
    warn!(
        "Stealth profile reloaded: cli={} sdk={} os={}/{}",
        profile.cli_version, profile.sdk_version, profile.stainless_os, profile.stainless_arch
    );
    global_profile().store(Arc::new(profile));
}
