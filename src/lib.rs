use std::{path::PathBuf, sync::LazyLock};

use clap::{Parser, Subcommand};
use colored::Colorize;

use crate::config::CLEWDR_CONFIG;

pub mod api;
pub mod billing;
pub mod claude_code_state;
pub mod cli;
pub mod config;
pub mod db;
pub mod error;
pub mod middleware;
pub mod oauth;
pub mod providers;
pub mod router;
pub mod services;
pub mod session;
pub mod state;
pub mod stealth;
pub mod types;
pub mod utils;

pub const IS_DEBUG: bool = cfg!(debug_assertions);
pub static IS_DEV: LazyLock<bool> = LazyLock::new(|| std::env::var("CARGO_MANIFEST_DIR").is_ok());

pub static VERSION_INFO: LazyLock<String> =
    LazyLock::new(|| format!("v{}", env!("CARGO_PKG_VERSION")));

/// Process-wide parsed CLI arguments. Initialised on first access.
///
/// Touching this LazyLock has no side effects beyond `argv` parsing — it is
/// safe to read from anywhere, including before logging initialisation.
pub static ARGS: LazyLock<Args> = LazyLock::new(Args::parse);

/// Returns version info with colors for terminal output
pub fn version_info_colored() -> String {
    format!(
        "v{}\n| profile: {}\n| mode: {}\n| no_fs: {}",
        env!("CARGO_PKG_VERSION"),
        if IS_DEBUG {
            "debug".yellow()
        } else {
            "release".green()
        },
        if *IS_DEV {
            "dev".yellow()
        } else {
            "prod".green()
        },
        if CLEWDR_CONFIG.load().no_fs {
            "true".yellow()
        } else {
            "false".green()
        }
    )
}

pub const FIG: &str = r#"
    //   ) )                                    //   ) )
   //        //  ___                   ___   / //___/ /
  //        // //___) ) //  / /  / / //   ) / / ___ (
 //        // //       //  / /  / / //   / / //   | |
((____/ / // ((____   ((__( (__/ / ((___/ / //    | |
"#;

/// Reverse Proxy API for Claude
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[cfg(feature = "portable")]
    /// Force update of the application
    #[arg(short, long, global = true)]
    pub update: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Path-related flags shared by every subcommand and the bare `serve` entry.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct GlobalOpts {
    /// load cookie from file (deprecated)
    #[arg(short, long, global = true)]
    pub file: Option<PathBuf>,

    /// Alternative config file
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Alternative log directory
    #[arg(short, long, global = true)]
    pub log_dir: Option<PathBuf>,

    /// Alternative database path
    #[arg(short = 'd', long, global = true)]
    pub db: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Run the HTTP server (default when no subcommand is given).
    Serve,

    /// Reset the admin user's password (requires existing database).
    ResetAdminPassword(cli::reset::Args),

    /// Export config and configuration tables to a portable bundle.
    ExportConfig(cli::export::Args),

    /// Import a previously exported bundle.
    ImportConfig(cli::import::Args),

    /// Read-only health check.
    Diagnose(cli::diagnose::Args),

    /// Show running state, port, and version.
    Status(cli::status::Args),

    /// Manage service registration (systemd / Termux:Boot).
    #[command(subcommand)]
    Service(cli::service::ServiceCommand),

    /// Interactive TUI menu.
    #[cfg(feature = "tui")]
    Menu,

    /// Force a one-shot self-update (alias of `--update`).
    #[cfg(feature = "portable")]
    Update,
}
