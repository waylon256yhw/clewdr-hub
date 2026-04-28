//! `clewdr import-config` — restore from a bundle in one transaction.

use std::path::PathBuf;

use crate::error::ClewdrError;

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum Mode {
    /// UPSERT each row by its UNIQUE key. Preserves rows not in the bundle.
    Merge,
    /// Truncate each config-like table, then re-insert from bundle.
    /// Requires `--yes` to confirm; admin user is preserved unless `--overwrite-admin`.
    Restore,
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Bundle path produced by `export-config`.
    pub path: PathBuf,

    /// Conflict strategy. Defaults to `merge`.
    #[arg(long, value_enum, default_value_t = Mode::Merge)]
    pub mode: Mode,

    /// Required when `--mode restore`. Acknowledges that target tables will be truncated.
    #[arg(long)]
    pub yes: bool,

    /// Overwrite the existing admin row even if its username matches.
    #[arg(long)]
    pub overwrite_admin: bool,

    /// Skip the actual writes; report what would change.
    #[arg(long)]
    pub dry_run: bool,

    /// Allow opening (creating) a fresh database. By default, import refuses on a missing DB.
    #[arg(long)]
    pub init: bool,

    /// Bypass minor version mismatch refusal. Major mismatch is always refused.
    #[arg(long)]
    pub force: bool,

    /// Read decryption passphrase from stdin instead of `/dev/tty`.
    #[arg(long)]
    pub passphrase_stdin: bool,
}

pub async fn run(_args: Args) -> Result<(), ClewdrError> {
    unimplemented!("import-config is implemented in commit #7")
}
