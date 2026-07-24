use std::{
    fmt::{Debug, Display},
    net::{IpAddr, SocketAddr},
};

use colored::Colorize;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use tokio::spawn;
use tracing::error;
use url::Url;
use wreq::Proxy;

use super::{CONFIG_PATH, ENDPOINT_URL};
use crate::{
    ARGS,
    config::{CC_CLIENT_ID, default_check_update, default_ip, default_port},
    error::ClewdrError,
};

/// Default trusted reverse-proxy CIDRs. See `trusted_proxies` field for semantics.
fn default_trusted_proxies() -> Vec<IpNet> {
    ["127.0.0.0/8", "::1/128", "172.16.0.0/12"]
        .iter()
        .map(|s| {
            s.parse()
                .expect("hard-coded default trusted_proxies CIDR must parse")
        })
        .collect()
}

fn default_non_stream_keepalive_interval_ms() -> u64 {
    6_000
}

fn default_non_stream_keepalive() -> bool {
    true
}

fn default_admin_session_ttl_hours() -> u64 {
    24
}

/// A struct representing the configuration of the application
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClewdrConfig {
    // Server settings, cannot hot reload
    #[serde(default = "default_ip")]
    ip: IpAddr,
    #[serde(default = "default_port")]
    port: u16,

    // App settings
    #[serde(default = "default_check_update")]
    pub check_update: bool,
    #[serde(default)]
    pub auto_update: bool,
    #[serde(default)]
    pub no_fs: bool,
    #[serde(default)]
    pub log_to_file: bool,
    #[serde(default)]
    pub debug_cookie: bool,
    /// Absolute lifetime of an admin login. Sessions are deliberately not
    /// renewed on activity, so this is also the maximum lifetime of a copied
    /// admin cookie.
    #[serde(default = "default_admin_session_ttl_hours")]
    pub admin_session_ttl_hours: u64,
    /// Add the Secure attribute to admin cookies. Keep this opt-in because
    /// local/LAN deployments commonly serve the panel over plain HTTP.
    #[serde(default)]
    pub admin_cookie_secure: bool,
    /// Bridge non-stream message requests through an upstream SSE response and
    /// periodically write JSON whitespace downstream. This keeps clients with
    /// per-read idle timeouts alive while preserving a final non-stream JSON
    /// document. May be overridden per request with
    /// `x-clewdr-non-stream-keepalive`.
    #[serde(default = "default_non_stream_keepalive")]
    pub non_stream_keepalive: bool,
    /// Whitespace heartbeat cadence for the non-stream bridge. Clamped to
    /// 250..=60_000 ms; `x-clewdr-non-stream-keepalive-interval-ms` can
    /// override it for one authenticated request.
    #[serde(default = "default_non_stream_keepalive_interval_ms")]
    pub non_stream_keepalive_interval_ms: u64,

    // Network settings
    #[serde(default)]
    pub proxy: Option<String>,

    /// CIDR list of reverse proxies whose `X-Forwarded-For` / `X-Real-IP`
    /// headers are trusted. When the TCP peer is in this list, the rightmost
    /// non-trusted hop in XFF (or X-Real-IP fallback) is taken as the client
    /// IP. Otherwise the peer address is used directly and forwarded headers
    /// are ignored — preventing spoofing by direct callers. Set to `[]` to
    /// disable header parsing entirely (always use peer IP).
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<IpNet>,

    // Claude Code settings
    #[serde(default)]
    pub claude_code_client_id: Option<String>,

    // Runtime proxy, not serialized
    #[serde(skip)]
    pub wreq_proxy: Option<Proxy>,
}

impl Default for ClewdrConfig {
    fn default() -> Self {
        Self {
            ip: default_ip(),
            port: default_port(),
            check_update: default_check_update(),
            auto_update: false,
            no_fs: false,
            log_to_file: false,
            debug_cookie: false,
            admin_session_ttl_hours: default_admin_session_ttl_hours(),
            admin_cookie_secure: false,
            non_stream_keepalive: default_non_stream_keepalive(),
            non_stream_keepalive_interval_ms: default_non_stream_keepalive_interval_ms(),
            proxy: None,
            trusted_proxies: default_trusted_proxies(),
            claude_code_client_id: None,
            wreq_proxy: None,
        }
    }
}

impl Display for ClewdrConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let endpoint = ENDPOINT_URL.to_string();
        writeln!(f, "Endpoint: {}", endpoint.green().underline())?;
        if let Some(ref proxy) = self.proxy {
            writeln!(f, "Proxy: {}", proxy.to_string().blue())?;
        }
        Ok(())
    }
}

impl ClewdrConfig {
    pub fn cc_client_id(&self) -> String {
        self.claude_code_client_id
            .as_deref()
            .unwrap_or(CC_CLIENT_ID)
            .to_string()
    }

    pub fn endpoint(&self) -> Url {
        ENDPOINT_URL.to_owned()
    }

    pub fn address(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }

    pub fn new() -> Self {
        let config: ClewdrConfig = Figment::from(Toml::file(CONFIG_PATH.as_path()))
            .admerge(Env::prefixed("CLEWDR_").split("__"))
            .extract_lossy()
            .inspect_err(|e| {
                error!("Failed to load config: {}", e);
            })
            .unwrap_or_default();
        if let Some(f) = ARGS.global.file.as_ref()
            && f.exists()
        {
            tracing::warn!("--file flag is deprecated; manage cookies via admin API instead");
        }
        let config = config.validate();
        if !config.no_fs {
            let config_clone = config.to_owned();
            spawn(async move {
                config_clone.save().await.unwrap_or_else(|e| {
                    error!("Failed to save config: {}", e);
                });
            });
        }
        config
    }

    pub async fn save(&self) -> Result<(), ClewdrError> {
        if self.no_fs {
            return Ok(());
        }
        if let Some(parent) = CONFIG_PATH.parent()
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(tokio::fs::write(CONFIG_PATH.as_path(), toml::ser::to_string_pretty(self)?).await?)
    }

    pub fn validate(mut self) -> Self {
        self.admin_session_ttl_hours = self.admin_session_ttl_hours.clamp(1, 168);
        self.non_stream_keepalive_interval_ms =
            self.non_stream_keepalive_interval_ms.clamp(250, 60_000);
        self.wreq_proxy = self.proxy.to_owned().and_then(|p| {
            Proxy::all(p)
                .inspect_err(|e| {
                    self.proxy = None;
                    error!("Failed to parse proxy: {}", e);
                })
                .ok()
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use super::ClewdrConfig;

    #[test]
    fn non_stream_keepalive_defaults_on_when_config_omits_it() {
        let config: ClewdrConfig = toml::from_str("").unwrap();
        assert!(config.non_stream_keepalive);
        assert_eq!(config.non_stream_keepalive_interval_ms, 6_000);
    }

    #[test]
    fn non_stream_keepalive_can_be_disabled_explicitly() {
        let config: ClewdrConfig = toml::from_str("non_stream_keepalive = false").unwrap();
        assert!(!config.non_stream_keepalive);
    }
}
