//! `clewdr menu` — interactive TUI wrapping the rest of the verbs.
//!
//! Every menu item gathers inputs through `inquire` prompts, builds the
//! same `Args` struct the equivalent CLI subcommand would parse from
//! argv, and dispatches into the underlying `cli::*::run()` entry
//! point. There is *no* business logic in this file: the menu is a
//! thin presentation layer, so anything `clewdr export-config /tmp/x`
//! can do, the menu's "Export config" item does identically. Plan §13
//! pin: don't duplicate logic; if the verb gains a flag, expose it
//! here, don't fork the implementation.
//!
//! The whole module is gated behind `--features tui` (default-on).

use std::{io::IsTerminal, path::PathBuf};

use colored::Colorize;
use inquire::{Confirm, InquireError, Select, Text};

use crate::{cli, error::ClewdrError};

pub async fn run() -> Result<(), ClewdrError> {
    // inquire's crossterm backend assumes a TTY for both stdin AND
    // stdout — if we're piped (e.g. `clewdr menu | cat` or invoked
    // from a service unit) the very first prompt errors with an
    // opaque crossterm IO failure that doesn't tell the user what
    // went wrong. Bail early with a hint that points them at
    // `--help` for the equivalent subcommands.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(ClewdrError::BadRequest {
            msg: "menu requires an interactive TTY for stdin and stdout — see `clewdr --help` for the equivalent subcommands",
        });
    }

    print_banner();

    loop {
        let action = match prompt_main_menu()? {
            Some(a) => a,
            // Esc / Ctrl-C at the top-level menu = exit cleanly.
            None => return Ok(()),
        };
        if matches!(action, MenuAction::Quit) {
            return Ok(());
        }
        // Each verb's own error reporting (export.rs, import.rs, …)
        // already covers the success path with its colored "✓" line,
        // so on Ok we just loop back to the menu. On Err we surface
        // the message but stay in the loop — a typo on one prompt
        // shouldn't kick the operator back to the shell.
        if let Err(e) = run_action(action).await {
            eprintln!("{} {}", "✗".red().bold(), e);
        }
        eprintln!();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    Status,
    Diagnose,
    ResetAdmin,
    ExportConfig,
    ImportConfig,
    ServiceInstall,
    ServiceUninstall,
    #[cfg(feature = "portable")]
    Update,
    Quit,
}

fn menu_entries() -> Vec<(&'static str, MenuAction)> {
    let mut entries: Vec<(&'static str, MenuAction)> = vec![
        ("Show status", MenuAction::Status),
        ("Diagnose (read-only health check)", MenuAction::Diagnose),
        ("Reset admin password", MenuAction::ResetAdmin),
        ("Export config to bundle", MenuAction::ExportConfig),
        ("Import config from bundle", MenuAction::ImportConfig),
        (
            "Install service (systemd / Termux:Boot)",
            MenuAction::ServiceInstall,
        ),
        ("Uninstall service", MenuAction::ServiceUninstall),
    ];
    #[cfg(feature = "portable")]
    entries.push(("Check for updates", MenuAction::Update));
    entries.push(("Quit", MenuAction::Quit));
    entries
}

/// Top-level Select prompt. Returns `Ok(None)` when the user cancels
/// (Esc/Ctrl-C) so the caller can exit cleanly without printing an
/// error.
fn prompt_main_menu() -> Result<Option<MenuAction>, ClewdrError> {
    let entries = menu_entries();
    let labels: Vec<&str> = entries.iter().map(|(l, _)| *l).collect();
    let result = Select::new("clewdr menu — pick an action:", labels.clone())
        .with_help_message("↑/↓ to navigate · enter to select · esc/Ctrl-C to quit")
        .prompt();
    match result {
        Ok(label) => {
            let action = entries
                .iter()
                .find(|(l, _)| *l == label)
                .map(|(_, a)| *a)
                .ok_or(ClewdrError::BadRequest {
                    msg: "selected menu label has no matching action (this is a bug)",
                })?;
            Ok(Some(action))
        }
        Err(e) if is_user_cancel(&e) => Ok(None),
        Err(e) => Err(wrap_inquire(e)),
    }
}

async fn run_action(action: MenuAction) -> Result<(), ClewdrError> {
    match action {
        MenuAction::Status => cli::status::run(cli::status::Args { json: false }).await,
        MenuAction::Diagnose => cli::diagnose::run(cli::diagnose::Args { json: false }).await,
        MenuAction::ResetAdmin => menu_reset_admin().await,
        MenuAction::ExportConfig => menu_export_config().await,
        MenuAction::ImportConfig => menu_import_config().await,
        MenuAction::ServiceInstall => {
            cli::service::run(cli::service::ServiceCommand::Install(
                cli::service::InstallArgs {
                    systemd: false,
                    termux_boot: false,
                },
            ))
            .await
        }
        MenuAction::ServiceUninstall => menu_service_uninstall().await,
        #[cfg(feature = "portable")]
        MenuAction::Update => cli::run_update().await,
        MenuAction::Quit => Ok(()), // handled by caller; here for completeness
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Per-verb prompt builders
// ──────────────────────────────────────────────────────────────────────────

async fn menu_reset_admin() -> Result<(), ClewdrError> {
    let username = Text::new("Username to reset:")
        .with_default("admin")
        .prompt()
        .map_err(wrap_or_cancel)?;
    // We deliberately pass `password: None` so cli::reset::run drives
    // its own rpassword prompts (with confirmation). Re-prompting here
    // would either duplicate that logic or create a path where the
    // password is read in a less-secure way than the CLI verb's.
    cli::reset::run(cli::reset::Args {
        password: None,
        from_env: false,
        username,
    })
    .await
}

async fn menu_export_config() -> Result<(), ClewdrError> {
    let path = Text::new("Bundle output path:")
        .with_help_message("e.g. ~/clewdr.bundle (~ is expanded)")
        .prompt()
        .map_err(wrap_or_cancel)?;
    // Same default as `clewdr export-config` (no `--no-encrypt`):
    // bundles ship encrypted unless the operator explicitly opts out.
    let encrypt = Confirm::new("Encrypt the bundle? (Argon2id KDF + AES-256-GCM)")
        .with_default(true)
        .prompt()
        .map_err(wrap_or_cancel)?;
    let no_secrets =
        Confirm::new("Strip secrets (cookies, OAuth tokens, password hashes, proxy creds)?")
            .with_default(false)
            .prompt()
            .map_err(wrap_or_cancel)?;
    let include_runtime = Confirm::new("Include runtime tables (usage rollups, account state)?")
        .with_default(false)
        .prompt()
        .map_err(wrap_or_cancel)?;
    cli::export::run(cli::export::Args {
        path: expand_tilde(&path),
        no_encrypt: !encrypt,
        no_secrets,
        include_runtime,
        passphrase_stdin: false,
    })
    .await
}

async fn menu_import_config() -> Result<(), ClewdrError> {
    let path = Text::new("Bundle path:")
        .with_help_message("absolute path or ~/clewdr.bundle")
        .prompt()
        .map_err(wrap_or_cancel)?;
    let mode_label = Select::new(
        "Conflict mode:",
        vec![
            "merge (UPSERT — preserves local rows that aren't in the bundle)",
            "restore (truncate & re-insert — destroys local data)",
        ],
    )
    .prompt()
    .map_err(wrap_or_cancel)?;
    let mode = if mode_label.starts_with("merge") {
        cli::import::Mode::Merge
    } else {
        cli::import::Mode::Restore
    };
    // restore wipes target tables; the CLI requires --yes for the same
    // reason. Surface that as an explicit prompt rather than silently
    // setting yes=true; the operator should consciously opt in.
    let yes = if mode == cli::import::Mode::Restore {
        let ok = Confirm::new("Restore wipes existing tables. Continue?")
            .with_default(false)
            .prompt()
            .map_err(wrap_or_cancel)?;
        if !ok {
            eprintln!("restore aborted");
            return Ok(());
        }
        true
    } else {
        false
    };
    let dry_run = Confirm::new("Dry run? (parse + report counts, no writes)")
        .with_default(true)
        .prompt()
        .map_err(wrap_or_cancel)?;
    let overwrite_admin = Confirm::new("Overwrite the local admin row from the bundle?")
        .with_default(false)
        .prompt()
        .map_err(wrap_or_cancel)?;
    cli::import::run(cli::import::Args {
        path: expand_tilde(&path),
        mode,
        yes,
        overwrite_admin,
        dry_run,
        init: false,
        force: false,
        passphrase_stdin: false,
    })
    .await
}

async fn menu_service_uninstall() -> Result<(), ClewdrError> {
    let purge = Confirm::new("--purge: also delete clewdr.db, clewdr.toml, and the log dir?")
        .with_default(false)
        .with_help_message(
            "Off by default; the verb will ask for `yes` confirmation if you say yes here",
        )
        .prompt()
        .map_err(wrap_or_cancel)?;
    cli::service::run(cli::service::ServiceCommand::Uninstall(
        cli::service::UninstallArgs {
            systemd: false,
            termux_boot: false,
            purge,
        },
    ))
    .await
}

// ──────────────────────────────────────────────────────────────────────────
// helpers
// ──────────────────────────────────────────────────────────────────────────

fn print_banner() {
    eprintln!(
        "{} {}",
        "clewdr".green().bold(),
        format!("v{}", env!("CARGO_PKG_VERSION")).dimmed()
    );
    eprintln!(
        "  {}",
        "operations menu — every action calls the same code path as the equivalent subcommand."
            .dimmed()
    );
    eprintln!();
}

/// Expand a leading `~/` to `$HOME` so operators can paste paths with
/// the conventional shorthand. Anything else passes through verbatim.
/// Pure — covered by `tests::expand_tilde_*`.
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        let mut buf = PathBuf::from(home);
        buf.push(rest);
        return buf;
    }
    PathBuf::from(s)
}

/// `OperationCanceled` (Esc) and `OperationInterrupted` (Ctrl-C) are
/// the user politely asking to back out — never propagate them as
/// errors that abort the program. Anything else is a real failure.
fn is_user_cancel(e: &InquireError) -> bool {
    matches!(
        e,
        InquireError::OperationCanceled | InquireError::OperationInterrupted
    )
}

/// Map an inquire error into a ClewdrError. Cancel/interrupt during a
/// sub-prompt becomes a benign "canceled" message rather than a hard
/// abort — the outer loop will redisplay the main menu.
fn wrap_or_cancel(e: InquireError) -> ClewdrError {
    if is_user_cancel(&e) {
        ClewdrError::BadRequest { msg: "canceled" }
    } else {
        wrap_inquire(e)
    }
}

fn wrap_inquire(e: InquireError) -> ClewdrError {
    ClewdrError::Whatever {
        message: format!("inquire: {e}"),
        source: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_substitutes_home() {
        // Force a known $HOME so the assertion is platform-independent.
        // SAFETY: tests run single-threaded by default for cargo test on
        // small crates, but be defensive — restore at end.
        let original = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", "/tmp/fake-home") };
        assert_eq!(
            expand_tilde("~/clewdr.bundle"),
            PathBuf::from("/tmp/fake-home/clewdr.bundle")
        );
        assert_eq!(
            expand_tilde("~/nested/dir/file"),
            PathBuf::from("/tmp/fake-home/nested/dir/file")
        );
        match original {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn expand_tilde_passes_through_absolute_paths() {
        // No tilde → no expansion, even if HOME is set.
        let p = expand_tilde("/etc/clewdr/bundle");
        assert_eq!(p, PathBuf::from("/etc/clewdr/bundle"));
    }

    #[test]
    fn expand_tilde_does_not_match_bare_tilde() {
        // Only `~/...` triggers expansion. `~user` (mid-string or no
        // slash) is left alone — we don't try to do shell-style user
        // lookups, only the common "$HOME shorthand" case.
        assert_eq!(expand_tilde("~"), PathBuf::from("~"));
        assert_eq!(expand_tilde("~user/foo"), PathBuf::from("~user/foo"));
    }

    #[test]
    fn menu_entries_include_quit_and_main_verbs() {
        let entries = menu_entries();
        let actions: Vec<MenuAction> = entries.iter().map(|(_, a)| *a).collect();
        // Pin the menu surface — anyone removing one of these actions
        // should think twice about what verb they're hiding from
        // operators who don't read --help.
        assert!(actions.contains(&MenuAction::Status));
        assert!(actions.contains(&MenuAction::Diagnose));
        assert!(actions.contains(&MenuAction::ResetAdmin));
        assert!(actions.contains(&MenuAction::ExportConfig));
        assert!(actions.contains(&MenuAction::ImportConfig));
        assert!(actions.contains(&MenuAction::ServiceInstall));
        assert!(actions.contains(&MenuAction::ServiceUninstall));
        assert!(actions.contains(&MenuAction::Quit));
        // Quit must be last so navigation order matches the user's
        // mental model ("escape hatch at the bottom").
        assert_eq!(entries.last().map(|(_, a)| *a), Some(MenuAction::Quit));
    }

    #[test]
    fn is_user_cancel_recognizes_esc_and_ctrl_c() {
        assert!(is_user_cancel(&InquireError::OperationCanceled));
        assert!(is_user_cancel(&InquireError::OperationInterrupted));
        // Other variants must NOT be treated as cancel — otherwise a
        // real IO error during prompting would silently exit the menu.
        let io_err = InquireError::IO(std::io::Error::other("not a cancel"));
        assert!(!is_user_cancel(&io_err));
    }
}
