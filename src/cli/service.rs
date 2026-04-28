//! `clewdr service install/uninstall` — register systemd unit (Linux) or
//! Termux:Boot script (Android/Termux) so the proxy comes back up after
//! a reboot.
//!
//! Two paths, picked by [`detect_environment`]:
//!
//! - **systemd**: writes `/etc/systemd/system/clewdr.service`, ensures a
//!   `clewdr` system user + `/opt/clewdr` workdir + `/opt/clewdr/log/`.
//!   Requires root. Uninstall reverses systemd state but leaves the
//!   user / workdir in place (other services may share them, and
//!   removing a system user is irreversible).
//! - **Termux:Boot**: writes `~/.termux/boot/clewdr-hub` with a
//!   non-blocking `nohup ... &` launcher. No root needed; assumes the
//!   Termux:Boot app is installed (we can't install it for the user,
//!   but we point at the F-Droid page if `~/.termux/boot/` is missing).
//!
//! `--purge` on uninstall additionally deletes the running config, db,
//! and log directory after an interactive confirmation. Default is
//! preserve. Non-TTY callers get a clear refusal so a script can't
//! accidentally wipe data.

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

    /// Also delete `clewdr.db`, `clewdr.toml`, and the log directory.
    /// Default is preserve. Requires an interactive TTY for confirmation.
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
    const SERVICE_USER: &str = "clewdr";
    const SERVICE_GROUP: &str = "clewdr";
    const WORKING_DIR: &str = "/opt/clewdr";
    const LOG_DIR: &str = "/opt/clewdr/log";
    const PURGE_DB_PATH: &str = "/opt/clewdr/clewdr.db";
    const PURGE_CONFIG_PATH: &str = "/opt/clewdr/clewdr.toml";
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

        // disable + stop is best-effort: systemctl returns non-zero
        // when the unit isn't loaded, which is exactly the "already
        // gone" case we want to tolerate. We still verify daemon-reload
        // succeeds at the end since that's load-bearing for systemd
        // forgetting the deleted unit.
        let _ = run_systemctl(&["disable", "--now", SYSTEMD_UNIT_NAME]);

        if Path::new(UNIT_PATH).exists() {
            fs::remove_file(UNIT_PATH)?;
        }
        run_systemctl(&["daemon-reload"])?;

        eprintln!(
            "{} clewdr.service disabled and unit file removed",
            "✓".green().bold()
        );
        eprintln!(
            "  System user '{SERVICE_USER}' + workdir '{WORKING_DIR}' left in place; \
             remove manually if not used by other services."
        );

        if purge {
            super::purge_data(&[
                Path::new(PURGE_DB_PATH),
                Path::new(PURGE_CONFIG_PATH),
                Path::new(LOG_DIR),
            ])?;
        }
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
             ExecStart={binary} --config {config} --db {db} --log-dir {log_dir}\n\
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
            config = PURGE_CONFIG_PATH,
            db = PURGE_DB_PATH,
            log_dir = LOG_DIR,
        )
    }

    fn require_root(action: &str) -> Result<(), ClewdrError> {
        if super::is_root() {
            return Ok(());
        }
        Err(ClewdrError::BadRequest {
            msg: Box::leak(
                format!(
                    "must run as root to {action} (try `sudo clewdr service install` / `... uninstall`)"
                )
                .into_boxed_str(),
            ),
        })
    }

    fn ensure_user_exists() -> Result<(), ClewdrError> {
        let exists = Command::new("getent")
            .args(["passwd", SERVICE_USER])
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
            return Err(ClewdrError::BadRequest {
                msg: Box::leak(
                    format!("systemctl {} returned non-zero", args.join(" ")).into_boxed_str(),
                ),
            });
        }
        Ok(())
    }

    fn current_exe() -> Result<PathBuf, ClewdrError> {
        env::current_exe().map_err(|e| ClewdrError::BadRequest {
            msg: Box::leak(format!("cannot determine current binary path: {e}").into_boxed_str()),
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
            return Err(ClewdrError::BadRequest {
                msg: Box::leak(
                    format!(
                        "~/.termux/boot/ not found — install the Termux:Boot app from F-Droid: {F_DROID_HINT}"
                    )
                    .into_boxed_str(),
                ),
            });
        }
        let script_path = boot_dir.join("clewdr-hub");
        let binary = env::current_exe().map_err(|e| ClewdrError::BadRequest {
            msg: Box::leak(format!("cannot resolve current exe: {e}").into_boxed_str()),
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

        if purge {
            // For Termux we don't have a privileged install path, so the
            // user's data lives wherever they unzipped the binary.
            // Best-effort guesses match the install.sh layout the plan
            // points at: $HOME/clewdr/ for binary + config + db,
            // $HOME/.local/clewdr/log/ for logs (matching the boot script
            // we just deleted).
            let guess_db = home.join("clewdr/clewdr.db");
            let guess_toml = home.join("clewdr/clewdr.toml");
            let log_dir = home.join(".local/clewdr/log");
            super::purge_data(&[&guess_db, &guess_toml, &log_dir])?;
        }
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

/// Interactive `--purge` confirmation. Refuses on a non-TTY because a
/// script accidentally piping `clewdr service uninstall --purge` should
/// not be able to silently delete the operator's data.
fn purge_data(paths: &[&Path]) -> Result<(), ClewdrError> {
    let existing: Vec<&Path> = paths.iter().copied().filter(|p| p.exists()).collect();
    if existing.is_empty() {
        eprintln!("  --purge: no clewdr.db / clewdr.toml / log dir found; nothing to delete.");
        return Ok(());
    }

    eprintln!();
    eprintln!("--purge will delete:");
    let mut total: u64 = 0;
    for path in &existing {
        let bytes = path_size(path);
        total += bytes;
        eprintln!("  {} ({})", path.display(), human_bytes(bytes).dimmed());
    }
    eprintln!("  total: {}", human_bytes(total).bold());

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

    for path in existing {
        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
        eprintln!("  removed {}", path.display());
    }
    Ok(())
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
                "ExecStart=/opt/clewdr/clewdr --config /opt/clewdr/clewdr.toml \
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
                "ExecStart=/usr/local/bin/clewdr --config /opt/clewdr/clewdr.toml \
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
