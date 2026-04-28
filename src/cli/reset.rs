//! `clewdr reset-admin-password` — overwrite the admin user's password.
//!
//! Resolves the new password from one of three sources (in priority order):
//! 1. `--password <pwd>` — direct value, intended for scripts (warns the
//!    operator about shell history exposure)
//! 2. `--from-env` — read from the `ADMIN_PASSWORD` environment variable
//! 3. interactive TTY prompt via `rpassword`, asked twice for confirmation
//!
//! The hash + DB UPDATE is delegated to [`db::reset_admin_password`], which
//! also bumps `session_version` so any active admin cookies become invalid.

use std::{io::IsTerminal, path::Path};

use colored::Colorize;

use crate::{config::DB_PATH, db, error::ClewdrError};

const ADMIN_PASSWORD_ENV: &str = "ADMIN_PASSWORD";
const PROMPT_HINT: &str = "Enter new admin password (input is hidden): ";
const CONFIRM_HINT: &str = "Confirm new admin password: ";

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

pub async fn run(args: Args) -> Result<(), ClewdrError> {
    let db_path = DB_PATH.to_owned();
    run_with_path(args, &db_path).await
}

/// Inner worker: takes the DB path explicitly so tests can target a tempdir
/// without going through the global [`DB_PATH`] LazyLock.
pub(crate) async fn run_with_path(args: Args, db_path: &Path) -> Result<(), ClewdrError> {
    eprintln!(
        "Resetting admin password in {}",
        db_path.display().to_string().blue()
    );

    let password = resolve_password(&args)?;

    // Argon2id is intentionally slow (~200ms on a phone). Keep it off the
    // tokio worker thread so we don't block other futures (cheap insurance
    // even though this binary path is single-task).
    let hash = tokio::task::spawn_blocking(move || db::hash_password_public(&password))
        .await
        .map_err(|e| ClewdrError::UnexpectedNone {
            msg: Box::leak(format!("argon2 task panicked: {e}").into_boxed_str()),
        })??;

    let pool = db::open_existing_pool(db_path).await?;
    db::reset_admin_password(&pool, &args.username, &hash).await?;
    pool.close().await;

    eprintln!(
        "{} admin password updated for user {}",
        "✓".green().bold(),
        args.username.bold()
    );
    eprintln!(
        "  All existing admin sessions are now invalidated; sign in again with the new password."
    );
    Ok(())
}

fn resolve_password(args: &Args) -> Result<String, ClewdrError> {
    if let Some(pwd) = args.password.as_deref() {
        if pwd.is_empty() {
            return Err(ClewdrError::BadRequest {
                msg: "--password cannot be empty",
            });
        }
        return Ok(pwd.to_string());
    }

    if args.from_env {
        return std::env::var(ADMIN_PASSWORD_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or(ClewdrError::BadRequest {
                msg: "ADMIN_PASSWORD env var is not set or empty",
            });
    }

    // Interactive path: require a real TTY so we don't deadlock waiting for
    // input that will never come (e.g., when invoked from a service unit).
    if !std::io::stdin().is_terminal() {
        return Err(ClewdrError::BadRequest {
            msg: "no TTY available — pass --password <pwd> or --from-env (with ADMIN_PASSWORD set)",
        });
    }

    let pwd = rpassword::prompt_password(PROMPT_HINT)?;
    if pwd.is_empty() {
        return Err(ClewdrError::BadRequest {
            msg: "password cannot be empty",
        });
    }
    let confirm = rpassword::prompt_password(CONFIRM_HINT)?;
    if pwd != confirm {
        return Err(ClewdrError::BadRequest {
            msg: "passwords do not match",
        });
    }
    Ok(pwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_with(password: Option<&str>, from_env: bool, username: &str) -> Args {
        Args {
            password: password.map(str::to_string),
            from_env,
            username: username.to_string(),
        }
    }

    #[test]
    fn resolve_password_uses_inline_password() {
        let args = args_with(Some("hunter2"), false, "admin");
        assert_eq!(resolve_password(&args).unwrap(), "hunter2");
    }

    #[test]
    fn resolve_password_rejects_empty_inline() {
        let args = args_with(Some(""), false, "admin");
        match resolve_password(&args) {
            Err(ClewdrError::BadRequest { msg }) => assert!(msg.contains("empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_with_path_updates_admin_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.db");

        let pool = db::init_pool(&path).await.unwrap();
        db::seed_admin(&pool).await.unwrap();
        let (old_hash, session_v_before): (String, i64) = sqlx::query_as(
            "SELECT password_hash, session_version FROM users WHERE username = 'admin'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        pool.close().await;

        run_with_path(args_with(Some("brand-new-pass"), false, "admin"), &path)
            .await
            .unwrap();

        let pool = db::open_existing_pool(&path).await.unwrap();
        let (new_hash, must_change, session_v_after): (String, i32, i64) = sqlx::query_as(
            "SELECT password_hash, must_change_password, session_version
             FROM users WHERE username = 'admin'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_ne!(new_hash, old_hash);
        assert_eq!(must_change, 0);
        assert_eq!(session_v_after, session_v_before + 1);
    }

    #[tokio::test]
    async fn run_with_path_errors_on_missing_db() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-existed.db");
        let res = run_with_path(args_with(Some("anything"), false, "admin"), &missing).await;
        assert!(matches!(res, Err(ClewdrError::DbNotFound { .. })));
        assert!(!missing.exists(), "the verb must not auto-create the DB");
    }

    #[tokio::test]
    async fn run_with_path_errors_when_admin_missing() {
        // DB exists with the schema but no admin user — reset should refuse
        // rather than silently re-seed one (which would mask DB corruption).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.db");
        let pool = db::init_pool(&path).await.unwrap();
        pool.close().await;

        let res = run_with_path(args_with(Some("anything"), false, "admin"), &path).await;
        match res {
            Err(ClewdrError::NotFound { msg }) => assert!(msg.contains("admin")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
