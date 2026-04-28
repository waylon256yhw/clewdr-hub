//! `clewdr export-config` — dump config + selected db tables to a portable bundle.

use std::path::PathBuf;

use crate::error::ClewdrError;

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Output bundle path.
    pub path: PathBuf,

    /// Write plaintext JSON instead of encrypting. Stamps the file 0600 and emits a stderr warning.
    #[arg(long)]
    pub no_encrypt: bool,

    /// Strip session cookies, OAuth refresh tokens, proxy passwords, and password hashes.
    #[arg(long)]
    pub no_secrets: bool,

    /// Include runtime tables (`account_runtime_state`, `usage_*`, `request_logs`).
    /// Off by default — these are large and not portable.
    #[arg(long)]
    pub include_runtime: bool,

    /// Read encryption passphrase from stdin instead of `/dev/tty`. Useful for scripts.
    #[arg(long, conflicts_with = "no_encrypt")]
    pub passphrase_stdin: bool,
}

pub async fn run(_args: Args) -> Result<(), ClewdrError> {
    unimplemented!("export-config is implemented in commit #6")
}
