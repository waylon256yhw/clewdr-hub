//! `clewdr status` — running state, port, version.

use crate::error::ClewdrError;

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(_args: Args) -> Result<(), ClewdrError> {
    unimplemented!("status is implemented in commit #5")
}
