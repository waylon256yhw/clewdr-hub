//! `clewdr reset-admin-password` — overwrite the admin user's password.

use crate::error::ClewdrError;

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// New password (skips interactive prompt). Avoid in shell history.
    #[arg(long)]
    pub password: Option<String>,

    /// Read password from `ADMIN_PASSWORD` environment variable instead of prompting.
    #[arg(long, conflicts_with = "password")]
    pub from_env: bool,

    /// Admin username to reset. Defaults to `admin`.
    #[arg(long, default_value = "admin")]
    pub username: String,
}

pub async fn run(_args: Args) -> Result<(), ClewdrError> {
    unimplemented!("reset-admin-password is implemented in commit #3")
}
