//! `clewdr diagnose` — read-only health check.

use crate::error::ClewdrError;

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Emit machine-readable JSON instead of human-readable colored output.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(_args: Args) -> Result<(), ClewdrError> {
    unimplemented!("diagnose is implemented in commit #4")
}
