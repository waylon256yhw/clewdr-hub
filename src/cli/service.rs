//! `clewdr service install/uninstall` — register systemd unit (Linux) or
//! Termux:Boot script (Android/Termux) so the proxy comes back up after
//! a reboot.
//!
//! Two paths, picked by [`detect_environment`]:
//!
//! - **systemd**: writes `/etc/systemd/system/clewdr.service`, ensures a
//!   `clewdr` system user + `/opt/clewdr` workdir + `/opt/clewdr/log/`.
//!   Requires root. Plain uninstall reverses systemd state and leaves
//!   data/binary/user in place; `--purge` does a complete teardown.
//! - **Termux:Boot**: writes `~/.termux/boot/clewdr-hub` with a
//!   non-blocking `nohup ... &` launcher. No root needed; assumes the
//!   Termux:Boot app is installed (we can't install it for the user,
//!   but we point at the F-Droid page if `~/.termux/boot/` is missing).
//!
//! `--purge` is a complete teardown: in addition to the data files, it
//! removes the binary, PATH symlink, workdir, systemd drop-in dir,
//! legacy logrotate config, and the `clewdr` system user + group
//! (Termux mirror covers the boot script, install dir, and PATH link).
//! After unlinking the running binary the verb finalizes and exits
//! cleanly so the menu can't loop in a half-uninstalled state.
//! Non-TTY callers get a clear refusal so a script can't accidentally
//! wipe an install.

use std::{
    env, fs,
    io::{IsTerminal, Write as _},
    path::{Path, PathBuf},
    process::Command,
};

use colored::Colorize;

use crate::error::ClewdrError;

const SYSTEMD_UNIT_NAME: &str = "clewdr.service";

#[derive(clap::Subcommand, Debug, Clone)]
pub enum ServiceCommand {
    /// Install the service registration (systemd unit or Termux:Boot script).
    Install(InstallArgs),
    /// Remove the service registration.
    Uninstall(UninstallArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct InstallArgs {
    /// Force the systemd path (skip Termux detection).
    #[arg(long, conflicts_with = "termux_boot")]
    pub systemd: bool,

    /// Force the Termux:Boot path (skip systemd detection).
    #[arg(long, conflicts_with = "systemd")]
    pub termux_boot: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct UninstallArgs {
    /// Force the systemd path (skip Termux detection).
    #[arg(long, conflicts_with = "termux_boot")]
    pub systemd: bool,

    /// Force the Termux:Boot path (skip systemd detection).
    #[arg(long, conflicts_with = "systemd")]
    pub termux_boot: bool,

    /// Complete teardown: remove the binary, PATH symlink, workdir,
    /// data files, systemd drop-in dir, legacy logrotate config, and
    /// the `clewdr` system user + group. Termux mirror covers the
    /// boot script, install dir, and PATH link. Default is preserve.
    /// Requires an interactive TTY for confirmation.
    #[arg(long)]
    pub purge: bool,
}

pub async fn run(cmd: ServiceCommand) -> Result<(), ClewdrError> {
    match cmd {
        ServiceCommand::Install(args) => {
            let env = detect_environment(args.systemd, args.termux_boot)?;
            match env {
                Environment::Systemd => systemd::install().await,
                Environment::TermuxBoot => termux::install().await,
            }
        }
        ServiceCommand::Uninstall(args) => {
            let env = detect_environment(args.systemd, args.termux_boot)?;
            match env {
                Environment::Systemd => systemd::uninstall(args.purge).await,
                Environment::TermuxBoot => termux::uninstall(args.purge).await,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Environment {
    Systemd,
    TermuxBoot,
}

/// Picks the install path. `--systemd` and `--termux-boot` short-circuit
/// auto-detection (and clap rejects passing both at once). Otherwise:
/// Termux is checked first because a Termux device may *also* have
/// systemd in some chroots, and the user expects Termux:Boot semantics
/// when running inside Termux.
fn detect_environment(force_systemd: bool, force_termux: bool) -> Result<Environment, ClewdrError> {
    if force_systemd {
        return Ok(Environment::Systemd);
    }
    if force_termux {
        return Ok(Environment::TermuxBoot);
    }
    if is_termux() {
        return Ok(Environment::TermuxBoot);
    }
    if has_systemd() {
        return Ok(Environment::Systemd);
    }
    Err(ClewdrError::BadRequest {
        msg: "no supported service manager found (need systemd on Linux or Termux:Boot on Android). \
              Override with --systemd or --termux-boot if auto-detection got it wrong.",
    })
}

fn is_termux() -> bool {
    if let Ok(prefix) = env::var("PREFIX")
        && (prefix.contains("com.termux") || prefix.contains("/data/data/com.termux"))
    {
        return true;
    }
    Path::new("/data/data/com.termux/files/usr").exists()
}

fn has_systemd() -> bool {
    Path::new("/run/systemd/system").exists()
}

// ──────────────────────────────────────────────────────────────────────────
// systemd
// ──────────────────────────────────────────────────────────────────────────

mod systemd {
    use super::*;

    const UNIT_PATH: &str = "/etc/systemd/system/clewdr.service";
    const UNIT_DROP_IN_DIR: &str = "/etc/systemd/system/clewdr.service.d";
    const LOGROTATE_PATH: &str = "/etc/logrotate.d/clewdr";
    const SERVICE_USER: &str = "clewdr";
    const SERVICE_GROUP: &str = "clewdr";
    const WORKING_DIR: &str = "/opt/clewdr";
    const LOG_DIR: &str = "/opt/clewdr/log";
    const SERVICE_DB_PATH: &str = "/opt/clewdr/clewdr.db";
    const SERVICE_CONFIG_PATH: &str = "/opt/clewdr/clewdr.toml";
    pub async fn install() -> Result<(), ClewdrError> {
        require_root("install systemd unit")?;
        ensure_user_exists()?;
        ensure_workdir()?;

        let binary = current_exe()?;
        let unit = unit_file_contents(&binary);
        write_unit_if_changed(&unit)?;

        run_systemctl(&["daemon-reload"])?;
        run_systemctl(&["enable", "--now", SYSTEMD_UNIT_NAME])?;

        eprintln!(
            "{} clewdr.service registered and started ({})",
            "✓".green().bold(),
            UNIT_PATH.dimmed()
        );
        eprintln!("  Check status: systemctl status clewdr");
        Ok(())
    }

    pub async fn uninstall(purge: bool) -> Result<(), ClewdrError> {
        require_root("uninstall systemd unit")?;

        let unit_existed = Path::new(UNIT_PATH).exists();

        // Always best-effort stop, even when the unit file is gone —
        // systemd may still have it loaded (manual rm without
        // daemon-reload, transient unit, etc.) and we don't want to
        // wipe data while the daemon keeps running. Quiet because the
        // "Unit not loaded" case is the expected no-op when there's
        // truly nothing to disable.
        let _ = run_systemctl_quiet(&["disable", "--now", SYSTEMD_UNIT_NAME]);

        if unit_existed {
            fs::remove_file(UNIT_PATH)?;
            run_systemctl(&["daemon-reload"])?;
            eprintln!(
                "{} clewdr.service disabled and unit file removed",
                "✓".green().bold()
            );
        } else {
            eprintln!("  no systemd unit at {} (already uninstalled?)", UNIT_PATH);
        }

        if !purge {
            eprintln!("  数据、二进制、系统用户保留；如需完全移除请加 --purge。");
            return Ok(());
        }

        let binary = env::current_exe().ok();
        let mut items: Vec<super::PurgeItem> = vec![
            super::PurgeItem {
                path: PathBuf::from(SERVICE_DB_PATH),
                label: "data",
            },
            super::PurgeItem {
                path: PathBuf::from(SERVICE_CONFIG_PATH),
                label: "config",
            },
            super::PurgeItem {
                path: PathBuf::from(LOG_DIR),
                label: "logs",
            },
            // Drop-in overrides: anything someone added under
            // /etc/systemd/system/clewdr.service.d/ (resource limits,
            // env tweaks, etc). Owning the dir name "clewdr.service.d"
            // makes this safe to recursively delete on purge.
            super::PurgeItem {
                path: PathBuf::from(UNIT_DROP_IN_DIR),
                label: "systemd drop-ins",
            },
            // Legacy logrotate config from the old README ("manual
            // systemd 持久化") that some operators copied into place by
            // hand. Current install paths don't deploy it, but
            // existing installs may carry it forward.
            super::PurgeItem {
                path: PathBuf::from(LOGROTATE_PATH),
                label: "logrotate",
            },
        ];

        // PATH symlink — only if it actually points at our binary, so
        // we never trash a custom `clewdr` someone else dropped at
        // /usr/local/bin/clewdr.
        let symlink = Path::new("/usr/local/bin/clewdr");
        if let Some(b) = &binary
            && let Ok(target) = fs::read_link(symlink)
        {
            let resolved = fs::canonicalize(&target).ok();
            let our = fs::canonicalize(b).ok();
            if resolved.is_some() && resolved == our {
                items.push(super::PurgeItem {
                    path: symlink.to_path_buf(),
                    label: "PATH link",
                });
            }
        }

        // Binary if it lives inside our managed workdir.
        let binary_in_workdir = binary.as_ref().is_some_and(|b| b.starts_with(WORKING_DIR));
        if binary_in_workdir && let Some(b) = &binary {
            items.push(super::PurgeItem {
                path: b.clone(),
                label: "binary",
            });
        }

        // Workdir last — we want everything inside it gone first so the
        // recursive remove_dir_all has nothing surprising to walk into.
        items.push(super::PurgeItem {
            path: PathBuf::from(WORKING_DIR),
            label: "workdir",
        });

        super::purge_all(items)?;

        // Best-effort: drop the service user and matching group.
        // userdel will refuse if any file still owns the uid (rare
        // after we wiped the workdir) or if a process tied to it
        // hasn't fully exited yet. groupdel as a separate call because
        // some distros don't auto-remove the primary group.
        let userdel_ok = Command::new("userdel")
            .arg(SERVICE_USER)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if userdel_ok {
            eprintln!("  removed system user {}", SERVICE_USER);
        }
        let _ = Command::new("groupdel")
            .arg(SERVICE_GROUP)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        super::finalize_if_self_removed(&binary);
        Ok(())
    }

    /// Generates the systemd unit content with the running binary path
    /// substituted. The data paths (`--config`, `--db`, `--log-dir`) are
    /// pinned to [`WORKING_DIR`] so the `clewdr` system user can actually
    /// write them — the running binary may live anywhere
    /// (`/usr/local/bin/clewdr`, `/opt/clewdr/clewdr`, …) and the
    /// `portable` feature otherwise resolves config/db relative to
    /// `current_exe().parent()`, which is typically root-owned and
    /// unwritable. Pure — covered by `tests::systemd_unit_file_*`.
    pub(super) fn unit_file_contents(binary: &Path) -> String {
        format!(
            "[Unit]\n\
             Description=clewdr-hub - Claude shared gateway\n\
             After=network.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             User={user}\n\
             Group={group}\n\
             WorkingDirectory={workdir}\n\
             ExecStart={binary} serve --config {config} --db {db} --log-dir {log_dir}\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             \n\
             Environment=CLEWDR_IP=0.0.0.0\n\
             Environment=CLEWDR_PORT=8484\n\
             \n\
             StandardOutput=journal\n\
             StandardError=journal\n\
             SyslogIdentifier=clewdr\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
            user = SERVICE_USER,
            group = SERVICE_GROUP,
            workdir = WORKING_DIR,
            binary = binary.display(),
            config = SERVICE_CONFIG_PATH,
            db = SERVICE_DB_PATH,
            log_dir = LOG_DIR,
        )
    }

    fn require_root(action: &str) -> Result<(), ClewdrError> {
        if super::is_root() {
            return Ok(());
        }
        Err(ClewdrError::BadRequestMessage {
            msg: format!(
                "must run as root to {action} (try `sudo clewdr service install` / `... uninstall`)"
            ),
        })
    }

    fn ensure_user_exists() -> Result<(), ClewdrError> {
        let exists = Command::new("getent")
            .args(["passwd", SERVICE_USER])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if exists {
            return Ok(());
        }
        let status = Command::new("useradd")
            .args([
                "--system",
                "--no-create-home",
                "--shell",
                "/sbin/nologin",
                SERVICE_USER,
            ])
            .status()?;
        if !status.success() {
            return Err(ClewdrError::BadRequest {
                msg: "failed to create system user 'clewdr' (useradd returned non-zero)",
            });
        }
        eprintln!(
            "{} created system user {}",
            "✓".green().bold(),
            SERVICE_USER.bold()
        );
        Ok(())
    }

    fn ensure_workdir() -> Result<(), ClewdrError> {
        for dir in [WORKING_DIR, LOG_DIR] {
            if !Path::new(dir).exists() {
                fs::create_dir_all(dir)?;
            }
            // Best-effort chown. Non-fatal if it fails (e.g., the workdir
            // is on a filesystem that doesn't carry POSIX ownership) —
            // the service will still start with whatever ownership the
            // dir already has, though writes may fail later. We surface
            // the failure as stderr noise rather than blowing up the
            // install for an edge case the user can fix manually.
            let _ = Command::new("chown")
                .args(["-R", &format!("{SERVICE_USER}:{SERVICE_GROUP}"), dir])
                .status();
        }
        Ok(())
    }

    fn write_unit_if_changed(unit: &str) -> Result<(), ClewdrError> {
        if let Ok(existing) = fs::read_to_string(UNIT_PATH)
            && existing == unit
        {
            return Ok(());
        }
        fs::write(UNIT_PATH, unit)?;
        Ok(())
    }

    fn run_systemctl(args: &[&str]) -> Result<(), ClewdrError> {
        let status = Command::new("systemctl").args(args).status()?;
        if !status.success() {
            return Err(ClewdrError::BadRequestMessage {
                msg: format!("systemctl {} returned non-zero", args.join(" ")),
            });
        }
        Ok(())
    }

    /// Best-effort systemctl call: silences both stdout and stderr so
    /// "Unit not loaded" / "does not exist" / "no such unit" never leak
    /// to the operator when we know the call is safe to fail (e.g.
    /// `disable --now` on a unit we're about to delete anyway). Returns
    /// success bool but most callers just ignore it.
    fn run_systemctl_quiet(args: &[&str]) -> bool {
        Command::new("systemctl")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn current_exe() -> Result<PathBuf, ClewdrError> {
        env::current_exe().map_err(|e| ClewdrError::BadRequestMessage {
            msg: format!("cannot determine current binary path: {e}"),
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Termux:Boot
// ──────────────────────────────────────────────────────────────────────────

mod termux {
    use super::*;

    const F_DROID_HINT: &str = "https://f-droid.org/packages/com.termux.boot/";

    pub async fn install() -> Result<(), ClewdrError> {
        let home = home_dir()?;
        let boot_dir = home.join(".termux/boot");
        if !boot_dir.exists() {
            return Err(ClewdrError::BadRequestMessage {
                msg: format!(
                    "~/.termux/boot/ not found — install the Termux:Boot app from F-Droid: {F_DROID_HINT}"
                ),
            });
        }
        let script_path = boot_dir.join("clewdr-hub");
        let binary = env::current_exe().map_err(|e| ClewdrError::BadRequestMessage {
            msg: format!("cannot resolve current exe: {e}"),
        })?;
        let log_dir = home.join(".local/clewdr/log");
        let contents = boot_script_contents(&binary, &log_dir);
        fs::write(&script_path, contents)?;
        make_executable(&script_path)?;

        eprintln!(
            "{} wrote {} (mode 0755)",
            "✓".green().bold(),
            script_path.display()
        );
        eprintln!(
            "  Termux:Boot will run this on next reboot. To start now without rebooting:\n  $ sh {}",
            script_path.display()
        );
        Ok(())
    }

    pub async fn uninstall(purge: bool) -> Result<(), ClewdrError> {
        let home = home_dir()?;
        let script_path = home.join(".termux/boot/clewdr-hub");
        if script_path.exists() {
            fs::remove_file(&script_path)?;
            eprintln!("{} removed {}", "✓".green().bold(), script_path.display());
        } else {
            eprintln!("  no boot script at {}; skipping", script_path.display());
        }
        eprintln!(
            "  if clewdr is still running, stop it manually (e.g. `pkill clewdr`) or restart Termux."
        );

        if !purge {
            eprintln!("  数据、二进制保留；如需完全移除请加 --purge。");
            return Ok(());
        }

        // Termux's install.sh layout puts everything under $HOME/clewdr/
        // (binary + config + db) and ~/.local/clewdr/log/ for logs.
        let install_dir = home.join("clewdr");
        let log_dir = home.join(".local/clewdr/log");
        let binary = env::current_exe().ok();

        let mut items: Vec<super::PurgeItem> = vec![
            super::PurgeItem {
                path: install_dir.join("clewdr.db"),
                label: "data",
            },
            super::PurgeItem {
                path: install_dir.join("clewdr.toml"),
                label: "config",
            },
            super::PurgeItem {
                path: log_dir,
                label: "logs",
            },
        ];

        // PATH symlink at $PREFIX/bin/clewdr — only if it points at us.
        if let Some(prefix) = env::var_os("PREFIX") {
            let symlink = PathBuf::from(prefix).join("bin/clewdr");
            if let Some(b) = &binary
                && let Ok(target) = fs::read_link(&symlink)
            {
                let resolved = fs::canonicalize(&target).ok();
                let our = fs::canonicalize(b).ok();
                if resolved.is_some() && resolved == our {
                    items.push(super::PurgeItem {
                        path: symlink,
                        label: "PATH link",
                    });
                }
            }
        }

        let binary_in_install = binary.as_ref().is_some_and(|b| b.starts_with(&install_dir));
        if binary_in_install && let Some(b) = &binary {
            items.push(super::PurgeItem {
                path: b.clone(),
                label: "binary",
            });
        }

        items.push(super::PurgeItem {
            path: install_dir,
            label: "install dir",
        });

        super::purge_all(items)?;

        // Try to remove ~/.local/clewdr/ if it's now empty (we just
        // wiped its only known child, the log dir). If something else
        // is in there — operator-placed files — rmdir refuses and
        // we leave it alone.
        let _ = fs::remove_dir(home.join(".local/clewdr"));

        super::finalize_if_self_removed(&binary);
        Ok(())
    }

    /// Generates the Termux:Boot launcher script. Pure — covered by
    /// `tests::termux_boot_script_*`.
    pub(super) fn boot_script_contents(binary: &Path, log_dir: &Path) -> String {
        // `nohup ... &` (not `exec ...`) is load-bearing here — the
        // Termux:Boot runner waits for the script to return; an
        // exec'd long-running process would make Android consider
        // the boot job hung and could withhold it on subsequent
        // boots. termux-wake-lock keeps the device from sleeping
        // out from under us before the proxy is up.
        format!(
            "#!/data/data/com.termux/files/usr/bin/sh\n\
             termux-wake-lock\n\
             mkdir -p \"{log_dir}\"\n\
             nohup \"{binary}\" serve >> \"{log_dir}/clewdr.boot.log\" 2>&1 &\n",
            log_dir = log_dir.display(),
            binary = binary.display(),
        )
    }

    fn home_dir() -> Result<PathBuf, ClewdrError> {
        env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(ClewdrError::BadRequest {
                msg: "HOME env var is not set; cannot locate ~/.termux/boot",
            })
    }

    /// chmod 0755 the just-written script. Cfg-gated because the
    /// `set_mode` API lives under `std::os::unix::fs` — Termux:Boot
    /// itself only runs on Android, but the crate must still compile
    /// on Windows targets that never reach this code at runtime.
    #[cfg(unix)]
    fn make_executable(path: &Path) -> Result<(), ClewdrError> {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) -> Result<(), ClewdrError> {
        // Termux is Android-only; reaching this branch means the user
        // forced --termux-boot on a non-unix host. The boot script we
        // just wrote isn't executable yet, but the script also can't
        // run on this OS, so the missing chmod isn't the problem here.
        // Surface a clear error rather than pretending we installed
        // something usable.
        Err(ClewdrError::BadRequest {
            msg: "Termux:Boot is only supported on Android/Unix; --termux-boot has no effect on this platform",
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Shared helpers
// ──────────────────────────────────────────────────────────────────────────

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

/// One entry in the `--purge` removal list. The label is shown next to
/// the path so the operator can tell at a glance what each line means
/// ("data" / "binary" / "PATH link" / "workdir" / …) before typing yes.
struct PurgeItem {
    path: PathBuf,
    label: &'static str,
}

/// Show the full purge preview, ask `yes` to confirm, then delete each
/// entry in order. Symlinks are unlinked themselves (not their targets);
/// directories are removed recursively (`remove_dir_all`); files via
/// `remove_file`. Caller is expected to have built `items` in an order
/// safe to delete top-to-bottom — typically with the workdir LAST.
fn purge_all(items: Vec<PurgeItem>) -> Result<(), ClewdrError> {
    // symlink_metadata so a dangling/managed symlink still counts as
    // "present" and gets cleaned, and the binary's data isn't followed
    // when we just want to remove the link.
    let existing: Vec<PurgeItem> = items
        .into_iter()
        .filter(|i| fs::symlink_metadata(&i.path).is_ok())
        .collect();
    if existing.is_empty() {
        eprintln!("  --purge: nothing to remove (already gone).");
        return Ok(());
    }

    let label_width = existing.iter().map(|i| i.label.len()).max().unwrap_or(0);

    eprintln!();
    eprintln!("--purge will remove:");
    for item in &existing {
        let hint = match fs::symlink_metadata(&item.path) {
            Ok(m) if m.file_type().is_symlink() => "symlink".to_string(),
            Ok(m) if m.is_file() => human_bytes(m.len()),
            Ok(_) => human_bytes(path_size(&item.path)),
            Err(_) => "?".to_string(),
        };
        eprintln!(
            "  {:<width$}  {}  {}",
            item.label,
            item.path.display(),
            format!("({hint})").dimmed(),
            width = label_width,
        );
    }

    if !std::io::stdin().is_terminal() {
        return Err(ClewdrError::BadRequest {
            msg: "no TTY for --purge confirmation; re-run interactively or omit --purge",
        });
    }
    eprint!("Type 'yes' to continue: ");
    std::io::stderr().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    if buf.trim() != "yes" {
        return Err(ClewdrError::BadRequest {
            msg: "purge aborted (response was not 'yes')",
        });
    }

    let mut failures: Vec<PathBuf> = Vec::new();
    for item in &existing {
        let result = match fs::symlink_metadata(&item.path) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&item.path),
            // Files AND symlinks both go through remove_file, which on
            // a symlink unlinks the link itself and never touches the
            // target — exactly what we want for the PATH link.
            Ok(_) => fs::remove_file(&item.path),
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => eprintln!("  removed {}", item.path.display()),
            Err(e) => {
                eprintln!(
                    "  {} failed to remove {}: {}",
                    "!".yellow().bold(),
                    item.path.display(),
                    e
                );
                failures.push(item.path.clone());
            }
        }
    }
    // Surface partial-failure to the caller so it doesn't proceed to
    // user/group removal or print a "✓ 已从本机移除" banner over a
    // half-cleaned install. The operator can re-run --purge once
    // they've fixed the cause (immutable bit, busy mount, ro fs, …).
    if !failures.is_empty() {
        return Err(ClewdrError::BadRequestMessage {
            msg: format!(
                "purge incomplete: {} item(s) failed to remove (see preceding lines for details)",
                failures.len()
            ),
        });
    }
    Ok(())
}

/// If we just unlinked the running binary, the menu loop / shell would
/// be staring at a half-uninstalled state — `clewdr` is no longer on
/// disk but the in-memory process keeps going. Print a final summary
/// and exit cleanly so neither the menu nor the verb caller continues.
fn finalize_if_self_removed(binary: &Option<PathBuf>) {
    let Some(b) = binary else { return };
    if b.exists() {
        return;
    }
    eprintln!();
    eprintln!("{} clewdr 已从本机移除", "✓".green().bold());
    use std::io::Write;
    std::io::stderr().flush().ok();
    std::io::stdout().flush().ok();
    std::process::exit(0);
}

fn path_size(p: &Path) -> u64 {
    let Ok(meta) = fs::metadata(p) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if meta.is_dir() {
        let Ok(entries) = fs::read_dir(p) else {
            return 0;
        };
        return entries.flatten().map(|e| path_size(&e.path())).sum();
    }
    0
}

fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.2} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.2} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_environment_respects_force_flags() {
        // --systemd and --termux-boot conflict via clap, so we only
        // need to pin that each one wins independently of the other
        // detection inputs (which are dynamic on the host).
        assert_eq!(
            detect_environment(true, false).unwrap(),
            Environment::Systemd
        );
        assert_eq!(
            detect_environment(false, true).unwrap(),
            Environment::TermuxBoot
        );
    }

    #[test]
    fn systemd_unit_file_pins_paths_and_substitutes_binary() {
        let unit = systemd::unit_file_contents(Path::new("/opt/clewdr/clewdr"));
        // ExecStart must point config + db + log_dir at the workdir
        // we chowned for `clewdr`. Without --config / --db the binary
        // would otherwise resolve them via current_exe().parent()
        // under the `portable` feature, which is typically root-owned
        // and unwritable by the service user (review #10 P1).
        assert!(
            unit.contains(
                "ExecStart=/opt/clewdr/clewdr serve --config /opt/clewdr/clewdr.toml \
                 --db /opt/clewdr/clewdr.db --log-dir /opt/clewdr/log"
            ),
            "unit:\n{unit}"
        );
        assert!(unit.contains("WorkingDirectory=/opt/clewdr"));
        assert!(unit.contains("User=clewdr"));
        assert!(unit.contains("Group=clewdr"));
        // Restart policy + journal identifier must survive future
        // template edits — they're load-bearing for `journalctl -u
        // clewdr` and crash recovery.
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("SyslogIdentifier=clewdr"));
        assert!(unit.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn systemd_unit_file_substitutes_alternate_binary_path() {
        // current_exe() returns the *test* binary path under cargo
        // test, so the template MUST honor whatever path it's handed
        // — not hardcode /opt/clewdr/clewdr. Crucially, a binary at
        // /usr/local/bin/clewdr still gets data paths inside
        // /opt/clewdr (review #10 P1: the `clewdr` system user can't
        // write under /usr/local/bin).
        let unit = systemd::unit_file_contents(Path::new("/usr/local/bin/clewdr"));
        assert!(
            unit.contains(
                "ExecStart=/usr/local/bin/clewdr serve --config /opt/clewdr/clewdr.toml \
                 --db /opt/clewdr/clewdr.db --log-dir /opt/clewdr/log"
            ),
            "unit:\n{unit}"
        );
        // WorkingDirectory still pins to the systemd-mode default.
        assert!(unit.contains("WorkingDirectory=/opt/clewdr"));
    }

    #[test]
    fn systemd_unit_file_data_paths_match_purge_paths() {
        // The unit's --config / --db / --log-dir args must point at
        // the same paths the --purge flow tries to delete. Otherwise
        // an operator who switches binaries (e.g. portable -> deb)
        // could end up with --purge missing the actual on-disk data.
        let unit = systemd::unit_file_contents(Path::new("/opt/clewdr/clewdr"));
        assert!(unit.contains("/opt/clewdr/clewdr.toml"));
        assert!(unit.contains("/opt/clewdr/clewdr.db"));
        assert!(unit.contains("/opt/clewdr/log"));
    }

    #[test]
    fn termux_boot_script_uses_nohup_not_exec() {
        // exec would block the boot job and mark it hung; nohup ... &
        // detaches and lets Termux:Boot return immediately.
        let script = termux::boot_script_contents(
            Path::new("/data/data/com.termux/files/home/clewdr/clewdr"),
            Path::new("/data/data/com.termux/files/home/.local/clewdr/log"),
        );
        assert!(script.starts_with("#!/data/data/com.termux/files/usr/bin/sh\n"));
        assert!(script.contains("nohup "));
        assert!(script.contains(" &\n"));
        assert!(!script.contains("exec "));
        assert!(script.contains("termux-wake-lock"));
        assert!(script.contains(" serve "));
    }

    #[test]
    fn termux_boot_script_redirects_to_log_dir() {
        let script = termux::boot_script_contents(Path::new("/x/clewdr"), Path::new("/y/log"));
        // Logs must land somewhere writable — the boot script has no TTY,
        // and unredirected output gets dropped.
        assert!(
            script.contains(">> \"/y/log/clewdr.boot.log\" 2>&1"),
            "script:\n{script}"
        );
        // mkdir is part of the script (not assumed at install time)
        // because the user can wipe the log dir without re-running install.
        assert!(script.contains("mkdir -p \"/y/log\""));
    }

    #[test]
    fn human_bytes_picks_appropriate_unit() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert!(human_bytes(2 * 1024).contains("KiB"));
        assert!(human_bytes(5 * 1024 * 1024).contains("MiB"));
        assert!(human_bytes(2 * 1024 * 1024 * 1024).contains("GiB"));
    }

    #[test]
    fn path_size_walks_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        // Single file
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"abcde").unwrap();
        assert_eq!(path_size(&f), 5);
        // Directory with nested subdir
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"xy").unwrap();
        let total = path_size(dir.path());
        assert!(total >= 5 + 2, "expected ≥ 7 bytes, got {total}");
        // Missing path is 0, not an error — the prompt should display
        // "0 B" rather than abort.
        assert_eq!(path_size(Path::new("/no/such/path/at/all")), 0);
    }
}
