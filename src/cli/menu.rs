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
            msg: "菜单需要交互式终端 stdin/stdout；非交互环境请使用 `clewdr --help` 查看等价子命令",
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
        ("查看运行状态", MenuAction::Status),
        ("运行诊断（只读健康检查）", MenuAction::Diagnose),
        ("重置管理员密码", MenuAction::ResetAdmin),
        ("导出配置备份包", MenuAction::ExportConfig),
        ("导入配置备份包", MenuAction::ImportConfig),
        (
            "安装自启动服务（systemd / Termux:Boot）",
            MenuAction::ServiceInstall,
        ),
        ("卸载自启动服务", MenuAction::ServiceUninstall),
    ];
    #[cfg(feature = "portable")]
    entries.push(("检查更新", MenuAction::Update));
    entries.push(("退出", MenuAction::Quit));
    entries
}

/// Top-level Select prompt. Returns `Ok(None)` when the user cancels
/// (Esc/Ctrl-C) so the caller can exit cleanly without printing an
/// error.
fn prompt_main_menu() -> Result<Option<MenuAction>, ClewdrError> {
    let entries = menu_entries();
    let labels: Vec<&str> = entries.iter().map(|(l, _)| *l).collect();
    let result = Select::new("请选择要执行的操作：", labels.clone())
        .with_page_size(labels.len())
        .with_help_message("↑/↓ 选择 · Enter 确认 · Esc/Ctrl-C 退出")
        .prompt();
    match result {
        Ok(label) => {
            let action = entries
                .iter()
                .find(|(l, _)| *l == label)
                .map(|(_, a)| *a)
                .ok_or(ClewdrError::BadRequest {
                    msg: "选中的菜单项没有对应操作（这是程序错误）",
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
        MenuAction::Diagnose => menu_diagnose().await,
        MenuAction::ResetAdmin => menu_reset_admin().await,
        MenuAction::ExportConfig => menu_export_config().await,
        MenuAction::ImportConfig => menu_import_config().await,
        MenuAction::ServiceInstall => menu_service_install().await,
        MenuAction::ServiceUninstall => menu_service_uninstall().await,
        #[cfg(feature = "portable")]
        MenuAction::Update => menu_check_update().await,
        MenuAction::Quit => Ok(()), // handled by caller; here for completeness
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Per-verb prompt builders
// ──────────────────────────────────────────────────────────────────────────

async fn menu_diagnose() -> Result<(), ClewdrError> {
    // diagnose::run() exits the process on any FAIL — that's correct
    // for the CLI path (CI / shell scripts want to branch on exit
    // status), but it would silently kill the menu loop. Use the
    // non-exiting entry point and surface the failure as a normal
    // menu error so the operator can keep working.
    let any_fail = cli::diagnose::run_and_report(cli::diagnose::Args { json: false }).await;
    if any_fail {
        return Err(ClewdrError::BadRequest {
            msg: "诊断发现至少一个 FAIL，请查看上方报告",
        });
    }
    Ok(())
}

async fn menu_reset_admin() -> Result<(), ClewdrError> {
    let username = Text::new("要重置的用户名：")
        .with_default("admin")
        .prompt()
        .map_err(wrap_or_cancel)?;
    let password = Text::new("新的管理员密码（明文显示）：")
        .prompt()
        .map_err(wrap_or_cancel)?;
    if password.is_empty() {
        return Err(ClewdrError::BadRequest {
            msg: "密码不能为空",
        });
    }
    let confirm = Text::new("再次输入新密码（明文显示）：")
        .prompt()
        .map_err(wrap_or_cancel)?;
    if password != confirm {
        return Err(ClewdrError::BadRequest {
            msg: "两次输入的密码不一致",
        });
    }

    cli::reset::run(cli::reset::Args {
        password: Some(password),
        from_env: false,
        username,
    })
    .await
}

async fn menu_export_config() -> Result<(), ClewdrError> {
    let path = Text::new("备份包输出路径：")
        .with_help_message("例如 ~/clewdr.bundle（会展开 ~）")
        .prompt()
        .map_err(wrap_or_cancel)?;
    // Same default as `clewdr export-config` (no `--no-encrypt`):
    // bundles ship encrypted unless the operator explicitly opts out.
    let encrypt = Confirm::new("是否加密备份包？（Argon2id KDF + AES-256-GCM）")
        .with_default(true)
        .prompt()
        .map_err(wrap_or_cancel)?;
    let no_secrets = Confirm::new("是否移除敏感信息？（cookies、OAuth token、密码哈希、代理凭据）")
        .with_default(false)
        .prompt()
        .map_err(wrap_or_cancel)?;
    let include_runtime = Confirm::new("是否包含运行时表？（用量统计、账号状态）")
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
    let path = Text::new("备份包路径：")
        .with_help_message("绝对路径或 ~/clewdr.bundle")
        .prompt()
        .map_err(wrap_or_cancel)?;
    let mode_label = Select::new(
        "导入模式：",
        vec![
            "合并（UPSERT；保留备份包中不存在的本地数据）",
            "恢复（清空并重建；会删除本地数据）",
        ],
    )
    .prompt()
    .map_err(wrap_or_cancel)?;
    let mode = if mode_label.starts_with("合并") {
        cli::import::Mode::Merge
    } else {
        cli::import::Mode::Restore
    };
    // restore wipes target tables; the CLI requires --yes for the same
    // reason. Surface that as an explicit prompt rather than silently
    // setting yes=true; the operator should consciously opt in.
    let yes = if mode == cli::import::Mode::Restore {
        let ok = Confirm::new("恢复模式会清空现有表，确定继续？")
            .with_default(false)
            .prompt()
            .map_err(wrap_or_cancel)?;
        if !ok {
            eprintln!("已取消恢复");
            return Ok(());
        }
        true
    } else {
        false
    };
    let dry_run = Confirm::new("是否只演练？（仅解析并报告数量，不写入）")
        .with_default(true)
        .prompt()
        .map_err(wrap_or_cancel)?;
    let overwrite_admin = Confirm::new("是否用备份包中的 admin 覆盖本地管理员？")
        .with_default(false)
        .prompt()
        .map_err(wrap_or_cancel)?;
    // --init: allow opening (creating) a fresh DB. Default off so a
    // typo'd --db path doesn't silently spawn an empty database. When
    // intentionally importing into a brand-new install (e.g. fresh
    // VPS, no clewdr.db yet), the operator wants this on.
    let init = Confirm::new("如果目标数据库不存在，是否新建？（--init）")
        .with_default(false)
        .with_help_message("关闭 = 目标 DB 不存在就拒绝导入；新安装机器可打开。")
        .prompt()
        .map_err(wrap_or_cancel)?;
    // --force: bypass the same-major / newer-minor refusal. Major
    // mismatch is *always* refused regardless of --force, so this
    // only loosens the minor-bump check.
    let force = Confirm::new("是否忽略小版本不匹配？（--force）")
        .with_default(false)
        .with_help_message(
            "来自更新小版本的备份包默认会被拒绝；--force 可覆盖。大版本不匹配始终拒绝。",
        )
        .prompt()
        .map_err(wrap_or_cancel)?;
    cli::import::run(cli::import::Args {
        path: expand_tilde(&path),
        mode,
        yes,
        overwrite_admin,
        dry_run,
        init,
        force,
        passphrase_stdin: false,
    })
    .await
}

async fn menu_service_install() -> Result<(), ClewdrError> {
    let (systemd, termux_boot) = prompt_service_target()?;
    cli::service::run(cli::service::ServiceCommand::Install(
        cli::service::InstallArgs {
            systemd,
            termux_boot,
        },
    ))
    .await
}

async fn menu_service_uninstall() -> Result<(), ClewdrError> {
    let (systemd, termux_boot) = prompt_service_target()?;
    let purge = Confirm::new("--purge：是否同时删除 clewdr.db、clewdr.toml 和日志目录？")
        .with_default(false)
        .with_help_message("默认关闭；如果这里选择是，后续仍会要求输入 `yes` 二次确认。")
        .prompt()
        .map_err(wrap_or_cancel)?;
    cli::service::run(cli::service::ServiceCommand::Uninstall(
        cli::service::UninstallArgs {
            systemd,
            termux_boot,
            purge,
        },
    ))
    .await
}

#[cfg(feature = "portable")]
async fn menu_check_update() -> Result<(), ClewdrError> {
    let updater = crate::services::update::ClewdrUpdater::new()?;
    let status = updater.check_update_status().await?;

    eprintln!("当前版本：v{}", status.current_version);
    eprintln!("最新版本：v{}", status.latest_version);
    if !status.update_available {
        eprintln!("结论：已是最新版本。");
        return Ok(());
    }

    eprintln!("结论：发现新版本。");
    let update_now =
        Confirm::new("是否立即下载并替换当前程序？更新成功后会自动退出，请重新进入菜单。")
            .with_default(true)
            .prompt()
            .map_err(wrap_or_cancel)?;
    if !update_now {
        eprintln!("已取消更新。");
        return Ok(());
    }

    eprintln!("正在下载并替换当前程序...");
    if !updater.update_to_latest().await? {
        eprintln!("更新检查结果已变化：当前已是最新版本。");
    }
    Ok(())
}

/// Resolve the (systemd, termux_boot) flag pair for the service verbs.
/// Auto-detect is the right answer 99% of the time; the explicit
/// overrides exist for edge cases the underlying error message itself
/// flags ("auto-detection got it wrong"). Returning false/false means
/// "let detect_environment() decide".
fn prompt_service_target() -> Result<(bool, bool), ClewdrError> {
    let label = Select::new(
        "服务目标：",
        vec![
            "自动检测（推荐）",
            "强制使用 systemd",
            "强制使用 Termux:Boot",
        ],
    )
    .with_help_message("只有自动检测选错时才需要覆盖，例如 Termux chroot 内的 systemd。")
    .prompt()
    .map_err(wrap_or_cancel)?;
    Ok(parse_service_target(&label))
}

/// Pure label → (systemd, termux_boot) mapping for the service-target
/// Select. Factored out so the mapping can be unit-tested without
/// driving the inquire prompt from a TTY.
fn parse_service_target(label: &str) -> (bool, bool) {
    if label.starts_with("强制使用 systemd") {
        (true, false)
    } else if label.starts_with("强制使用 Termux") {
        (false, true)
    } else {
        (false, false)
    }
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
        ClewdrError::BadRequest { msg: "已取消" }
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

    #[test]
    fn parse_service_target_maps_each_label() {
        // Auto-detect is the default — both flags off so
        // detect_environment() runs.
        assert_eq!(parse_service_target("自动检测（推荐）"), (false, false));
        // Override labels must turn on exactly one flag. A future edit
        // that breaks the label prefix would silently fall through to
        // auto-detect; this test catches that.
        assert_eq!(parse_service_target("强制使用 systemd"), (true, false));
        assert_eq!(parse_service_target("强制使用 Termux:Boot"), (false, true));
        // Anything unrecognised falls through to auto-detect (safest
        // default).
        assert_eq!(parse_service_target("garbage"), (false, false));
    }
}
