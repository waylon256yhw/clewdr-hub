//! `clewdr service install/uninstall` — register systemd unit or Termux:Boot script.

use crate::error::ClewdrError;

#[derive(clap::Subcommand, Debug, Clone)]
pub enum ServiceCommand {
    /// Install the service registration (systemd unit or Termux:Boot script).
    Install(InstallArgs),
    /// Remove the service registration.
    Uninstall(UninstallArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct InstallArgs {
    /// Force systemd path (skip Termux detection).
    #[arg(long, conflicts_with = "termux_boot")]
    pub systemd: bool,

    /// Force Termux:Boot path (skip systemd detection).
    #[arg(long, conflicts_with = "systemd")]
    pub termux_boot: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct UninstallArgs {
    /// Also delete `clewdr.db`, `clewdr.toml`, and the log directory.
    /// By default uninstall only removes the service registration.
    #[arg(long)]
    pub purge: bool,
}

pub async fn run(cmd: ServiceCommand) -> Result<(), ClewdrError> {
    match cmd {
        ServiceCommand::Install(args) => install(args).await,
        ServiceCommand::Uninstall(args) => uninstall(args).await,
    }
}

pub async fn install(_args: InstallArgs) -> Result<(), ClewdrError> {
    unimplemented!("service install is implemented in commit #10")
}

pub async fn uninstall(_args: UninstallArgs) -> Result<(), ClewdrError> {
    unimplemented!("service uninstall is implemented in commit #10")
}
