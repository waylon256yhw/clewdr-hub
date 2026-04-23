use std::{
    fmt::{Debug, Display},
    net::{IpAddr, SocketAddr},
};

use clap::Parser;
use colored::Colorize;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};
use tokio::spawn;
use tracing::error;
use url::Url;
use wreq::Proxy;

use super::{CONFIG_PATH, ENDPOINT_URL};
use crate::{
    Args,
    config::{CC_CLIENT_ID, default_check_update, default_ip, default_port},
    error::ClewdrError,
};

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

    // Network settings
    #[serde(default)]
    pub proxy: Option<String>,

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
            proxy: None,
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
        if let Some(ref f) = Args::try_parse().ok().and_then(|a| a.file)
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
