//! CLI subcommand handlers.
//!
//! Each verb lives in its own submodule and exposes:
//! - a `pub struct Args` (deriving `clap::Args`) embedded in [`crate::Command`]
//! - a `pub async fn run(args: Args) -> Result<(), ClewdrError>` entry point
//!
//! [`dispatch`] is invoked from `main.rs` *before* logging or [`crate::config::CLEWDR_CONFIG`]
//! initialization so that subcommands never race with the `tokio::spawn`-ed
//! config writeback in `ClewdrConfig::new()`.

pub mod diagnose;
pub mod export;
pub mod import;
#[cfg(feature = "tui")]
pub mod menu;
pub mod reset;
pub mod service;
pub mod status;

use crate::{Command, error::ClewdrError};

/// Dispatches a parsed [`Command`] to its handler.
///
/// `Command::Serve` is not handled here; `main.rs` falls through to the server
/// path when `ARGS.command` is `None` or `Some(Command::Serve)`.
pub async fn dispatch(cmd: Command) -> Result<(), ClewdrError> {
    match cmd {
        Command::Serve => unreachable!("Serve is dispatched directly from main"),
        Command::ResetAdminPassword(args) => reset::run(args).await,
        Command::ExportConfig(args) => export::run(args).await,
        Command::ImportConfig(args) => import::run(args).await,
        Command::Diagnose(args) => diagnose::run(args).await,
        Command::Status(args) => status::run(args).await,
        Command::Service(sc) => service::run(sc).await,
        #[cfg(feature = "tui")]
        Command::Menu => menu::run().await,
        #[cfg(feature = "portable")]
        Command::Update => run_update().await,
    }
}

/// Force a one-shot self-update, regardless of the `check_update` config flag.
///
/// Used by both `clewdr --update` (legacy flag, dispatched from main) and
/// `clewdr update` (subcommand, dispatched via [`dispatch`]).
#[cfg(feature = "portable")]
pub async fn run_update() -> Result<(), ClewdrError> {
    println!("{}\n{}", crate::FIG, crate::version_info_colored());
    let updater = crate::services::update::ClewdrUpdater::new()?;
    let _ = updater.check_for_updates(true).await?;
    Ok(())
}
