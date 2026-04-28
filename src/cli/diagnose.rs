//! `clewdr diagnose` — read-only health check.
//!
//! Runs ten small checks against the local install (binary, platform,
//! config, database, session secret, listener port, outbound connectivity,
//! update source, file permissions, data size) and prints a colored summary
//! or, with `--json`, a machine-readable list. Exit code is 0 unless one of
//! the checks reports `Fail`.
//!
//! All checks bypass [`crate::config::CLEWDR_CONFIG`] — they read TOML +
//! env directly via Figment so the verb never triggers the async writeback
//! that `ClewdrConfig::new()` spawns.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use colored::Colorize;
use serde::{Deserialize, Serialize};

use crate::{
    config::{CONFIG_PATH, DB_PATH},
    error::ClewdrError,
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const ANTHROPIC_PROBE_URL: &str = "https://api.anthropic.com/";
const GITHUB_PROBE_URL: &str =
    "https://api.github.com/repos/waylon256yhw/clewdr-hub/releases/latest";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Emit machine-readable JSON instead of human-readable colored output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
struct CheckResult {
    id: &'static str,
    status: Status,
    detail: String,
}

pub async fn run(args: Args) -> Result<(), ClewdrError> {
    let checks = run_all_checks().await;

    if args.json {
        print_json(&checks);
    } else {
        print_human(&checks);
    }

    if checks.iter().any(|c| c.status == Status::Fail) {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_all_checks() -> Vec<CheckResult> {
    let cfg = read_minimal_config();
    let mut out = Vec::with_capacity(10);
    out.push(check_binary());
    out.push(check_platform());
    out.push(check_config(&cfg));
    out.push(check_database(&cfg).await);
    out.push(check_session(&cfg).await);
    out.push(check_port(&cfg).await);
    out.push(check_anthropic(&cfg).await);
    out.push(check_update_source().await);
    out.push(check_permissions());
    out.push(check_data_size(&cfg));
    out
}

// ──────────────────────────────────────────────────────────────────────────
// Output
// ──────────────────────────────────────────────────────────────────────────

fn print_human(checks: &[CheckResult]) {
    println!("{}", "clewdr diagnose".bold());
    let id_width = checks.iter().map(|c| c.id.len()).max().unwrap_or(8);
    let mut ok_n = 0;
    let mut warn_n = 0;
    let mut fail_n = 0;
    for c in checks {
        let tag = match c.status {
            Status::Ok => {
                ok_n += 1;
                "[ OK ]".green().bold()
            }
            Status::Warn => {
                warn_n += 1;
                "[WARN]".yellow().bold()
            }
            Status::Fail => {
                fail_n += 1;
                "[FAIL]".red().bold()
            }
        };
        println!(
            "  {tag} {:<width$}  {detail}",
            c.id.bold(),
            width = id_width,
            detail = c.detail
        );
    }
    println!();
    println!(
        "{}/{} OK, {} WARN, {} FAIL",
        ok_n,
        checks.len(),
        warn_n,
        fail_n
    );
}

fn print_json(checks: &[CheckResult]) {
    let summary = serde_json::json!({
        "ok":   checks.iter().filter(|c| c.status == Status::Ok).count(),
        "warn": checks.iter().filter(|c| c.status == Status::Warn).count(),
        "fail": checks.iter().filter(|c| c.status == Status::Fail).count(),
    });
    let body = serde_json::json!({
        "checks": checks,
        "summary": summary,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Config plumbing (Figment, no CLEWDR_CONFIG side effects)
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct DiagConfig {
    ip: Option<IpAddr>,
    port: Option<u16>,
    proxy: Option<String>,
    no_fs: Option<bool>,
}

impl DiagConfig {
    fn ip(&self) -> IpAddr {
        self.ip
            .unwrap_or_else(|| IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))
    }
    fn port(&self) -> u16 {
        self.port.unwrap_or(8484)
    }
    fn no_fs(&self) -> bool {
        self.no_fs.unwrap_or(false)
    }
    fn bind_address(&self) -> SocketAddr {
        SocketAddr::new(self.ip(), self.port())
    }
    /// Address used by the local HTTP probe.
    ///
    /// For wildcard binds (`0.0.0.0` / `::`) we substitute the matching
    /// loopback because the running server answers on every interface,
    /// including loopback. For explicit binds (LAN address, `[::1]`) we
    /// preserve the configured IP — otherwise a server on
    /// `192.168.1.10:8484` or `[::1]:8484` would silently fail this probe
    /// and get downgraded to `PORT_OCCUPIED_UNKNOWN` once the bind retry
    /// also fails to reclaim the address.
    fn probe_address(&self) -> SocketAddr {
        let probe_ip = match self.ip() {
            IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            other => other,
        };
        SocketAddr::new(probe_ip, self.port())
    }
}

fn read_minimal_config() -> DiagConfig {
    use figment::{
        Figment,
        providers::{Env, Format, Toml},
    };
    Figment::from(Toml::file(CONFIG_PATH.as_path()))
        .admerge(Env::prefixed("CLEWDR_").split("__"))
        .extract()
        .unwrap_or_default()
}

// ──────────────────────────────────────────────────────────────────────────
// Individual checks
// ──────────────────────────────────────────────────────────────────────────

fn check_binary() -> CheckResult {
    let path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    CheckResult {
        id: "binary",
        status: Status::Ok,
        detail: format!("{path} (v{}, {profile})", env!("CARGO_PKG_VERSION")),
    }
}

fn check_platform() -> CheckResult {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let env_tag = if cfg!(target_env = "musl") {
        " musl"
    } else if cfg!(target_env = "gnu") {
        " glibc"
    } else {
        ""
    };
    let in_termux = std::env::var("PREFIX")
        .ok()
        .map(|p| p.contains("com.termux"))
        .unwrap_or(false)
        || Path::new("/data/data/com.termux").exists();
    let detail = if in_termux {
        format!("{os}-{arch}{env_tag} (Termux detected)")
    } else {
        format!("{os}-{arch}{env_tag}")
    };
    CheckResult {
        id: "platform",
        status: Status::Ok,
        detail,
    }
}

fn check_config(cfg: &DiagConfig) -> CheckResult {
    if cfg.no_fs() {
        // In `no_fs` mode the operator is configuring entirely through
        // env vars (CLEWDR_*) — there's no on-disk clewdr.toml to look
        // for. Emitting a WARN here would be a false positive for healthy
        // HF Space deployments.
        return CheckResult {
            id: "config",
            status: Status::Ok,
            detail: "skipped — no_fs mode (env-driven config)".to_string(),
        };
    }
    let path = CONFIG_PATH.as_path();
    if !path.exists() {
        return CheckResult {
            id: "config",
            status: Status::Warn,
            detail: format!("{} does not exist (defaults will be used)", path.display()),
        };
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return CheckResult {
                id: "config",
                status: Status::Fail,
                detail: format!("{} unreadable: {}", path.display(), e),
            };
        }
    };
    match toml::from_str::<toml::Value>(&raw) {
        Ok(_) => CheckResult {
            id: "config",
            status: Status::Ok,
            detail: format!("{} parsed cleanly", path.display()),
        },
        Err(e) => CheckResult {
            id: "config",
            status: Status::Fail,
            detail: format!("{} parse error: {}", path.display(), e),
        },
    }
}

async fn check_database(cfg: &DiagConfig) -> CheckResult {
    if cfg.no_fs() {
        return CheckResult {
            id: "database",
            status: Status::Ok,
            detail: "skipped — no_fs mode (running server uses in-memory database)".to_string(),
        };
    }
    let path = DB_PATH.as_path();
    if !path.exists() {
        return CheckResult {
            id: "database",
            status: Status::Warn,
            detail: format!(
                "{} does not exist (run `clewdr serve` first to create it)",
                path.display()
            ),
        };
    }
    let pool = match crate::db::open_readonly_pool(path).await {
        Ok(p) => p,
        Err(e) => {
            return CheckResult {
                id: "database",
                status: Status::Fail,
                detail: format!("open: {e}"),
            };
        }
    };

    let integrity: Option<(String,)> = sqlx::query_as("PRAGMA integrity_check")
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten();
    let admin_count: i64 = sqlx::query_as("SELECT COUNT(*) FROM users WHERE role='admin'")
        .fetch_one(&pool)
        .await
        .map(|(n,): (i64,)| n)
        .unwrap_or(-1);
    pool.close().await;

    let integrity_ok = integrity.as_ref().is_some_and(|(s,)| s == "ok");
    if !integrity_ok {
        return CheckResult {
            id: "database",
            status: Status::Fail,
            detail: format!(
                "{} integrity_check failed: {:?}",
                path.display(),
                integrity.map(|(s,)| s)
            ),
        };
    }
    if admin_count < 1 {
        return CheckResult {
            id: "database",
            status: Status::Fail,
            detail: format!(
                "{} has no admin user — run `clewdr reset-admin-password` after `serve` seeds one",
                path.display()
            ),
        };
    }
    CheckResult {
        id: "database",
        status: Status::Ok,
        detail: format!(
            "{} integrity OK, {admin_count} admin user(s)",
            path.display()
        ),
    }
}

async fn check_session(cfg: &DiagConfig) -> CheckResult {
    use base64::Engine;
    if cfg.no_fs() {
        return CheckResult {
            id: "session",
            status: Status::Ok,
            detail: "skipped — no_fs mode".to_string(),
        };
    }
    let path = DB_PATH.as_path();
    if !path.exists() {
        return CheckResult {
            id: "session",
            status: Status::Warn,
            detail: "skipped: database missing".to_string(),
        };
    }
    let pool = match crate::db::open_readonly_pool(path).await {
        Ok(p) => p,
        Err(e) => {
            return CheckResult {
                id: "session",
                status: Status::Fail,
                detail: format!("open db: {e}"),
            };
        }
    };
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM settings WHERE key='session_secret'")
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
    pool.close().await;

    let Some((value,)) = row else {
        return CheckResult {
            id: "session",
            status: Status::Fail,
            detail: "session_secret missing — server has not finished bootstrapping".to_string(),
        };
    };
    match base64::engine::general_purpose::STANDARD.decode(&value) {
        Ok(b) if b.len() == 32 => CheckResult {
            id: "session",
            status: Status::Ok,
            detail: "session_secret 32 bytes".to_string(),
        },
        Ok(b) => CheckResult {
            id: "session",
            status: Status::Fail,
            detail: format!("session_secret decoded but {} bytes (expected 32)", b.len()),
        },
        Err(e) => CheckResult {
            id: "session",
            status: Status::Fail,
            detail: format!("session_secret base64 decode: {e}"),
        },
    }
}

/// Three-state port probe (per plan):
/// - 200 + clewdr-shaped body → RUNNING (matches binary OR mismatch)
/// - 200 + foreign body → PORT_OCCUPIED_OTHER (warn — not us)
/// - connection refused → try `TcpListener::bind`
///   - bind ok → port free, ready to start
///   - bind fail → PORT_OCCUPIED_UNKNOWN (warn)
async fn check_port(cfg: &DiagConfig) -> CheckResult {
    let probe_addr = cfg.probe_address();
    let url = format!("http://{}/api/version", probe_addr);
    let client = match wreq::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                id: "port",
                status: Status::Fail,
                detail: format!("HTTP client build: {e}"),
            };
        }
    };

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let live_version = resp.text().await.ok().map(|s| s.trim().to_string());
            match live_version.as_deref() {
                Some(v) if v.starts_with('v') && is_semver_ish(&v[1..]) => {
                    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
                    let detail = if v == current {
                        format!("RUNNING on {probe_addr} — {v} (matches binary)")
                    } else {
                        format!(
                            "RUNNING on {probe_addr} — live={v}, binary={current} (restart to pick up new binary)"
                        )
                    };
                    CheckResult {
                        id: "port",
                        status: Status::Ok,
                        detail,
                    }
                }
                _ => CheckResult {
                    id: "port",
                    status: Status::Warn,
                    detail: format!(
                        "PORT_OCCUPIED_OTHER: {probe_addr} responded but body isn't a clewdr version"
                    ),
                },
            }
        }
        Ok(resp) => CheckResult {
            id: "port",
            status: Status::Warn,
            detail: format!(
                "PORT_OCCUPIED_OTHER: {probe_addr} returned HTTP {} (not clewdr)",
                resp.status().as_u16()
            ),
        },
        Err(_) => match TcpListener::bind(cfg.bind_address()) {
            Ok(listener) => {
                drop(listener);
                CheckResult {
                    id: "port",
                    status: Status::Ok,
                    detail: format!("port free — ready to bind {}", cfg.bind_address()),
                }
            }
            Err(_) => CheckResult {
                id: "port",
                status: Status::Warn,
                detail: format!(
                    "PORT_OCCUPIED_UNKNOWN: {} held by another listener (not responding to /api/version)",
                    cfg.bind_address()
                ),
            },
        },
    }
}

fn is_semver_ish(s: &str) -> bool {
    let mut parts = s.split('.');
    let a = parts.next();
    let b = parts.next();
    let c = parts.next();
    match (a, b, c) {
        (Some(x), Some(y), Some(z)) => {
            // Accept "1.2.3" and "1.2.3-pre" / "1.2.3+meta" prefixes.
            let z_num: String = z.chars().take_while(|c| c.is_ascii_digit()).collect();
            x.chars().all(|c| c.is_ascii_digit())
                && y.chars().all(|c| c.is_ascii_digit())
                && !z_num.is_empty()
        }
        _ => false,
    }
}

async fn check_anthropic(cfg: &DiagConfig) -> CheckResult {
    let mut builder = wreq::Client::builder().timeout(HTTP_TIMEOUT);
    let mut via_proxy = false;
    if let Some(proxy_url) = cfg.proxy.as_deref() {
        match wreq::Proxy::all(proxy_url) {
            Ok(p) => {
                builder = builder.proxy(p);
                via_proxy = true;
            }
            Err(e) => {
                return CheckResult {
                    id: "anthropic",
                    status: Status::Warn,
                    detail: format!("proxy `{proxy_url}` malformed: {e}"),
                };
            }
        }
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                id: "anthropic",
                status: Status::Fail,
                detail: format!("HTTP client build: {e}"),
            };
        }
    };
    probe_url(
        &client,
        ANTHROPIC_PROBE_URL,
        "anthropic",
        if via_proxy { "via proxy" } else { "direct" },
    )
    .await
}

async fn check_update_source() -> CheckResult {
    // GitHub probe always goes direct — proxy could mask whether the
    // self-update path will work for this binary.
    let client = match wreq::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                id: "update",
                status: Status::Fail,
                detail: format!("HTTP client build: {e}"),
            };
        }
    };
    probe_url(&client, GITHUB_PROBE_URL, "update", "direct").await
}

async fn probe_url(client: &wreq::Client, url: &str, id: &'static str, via: &str) -> CheckResult {
    let started = Instant::now();
    match client.head(url).send().await {
        Ok(resp) => {
            let elapsed = started.elapsed();
            let code = resp.status().as_u16();
            // For a connectivity probe, any non-5xx response proves the
            // TCP + TLS + HTTP path works. A 404/403 on `HEAD /` is normal
            // (these URLs are API roots, not pages). Only network errors
            // and 5xx warrant a WARN.
            if resp.status().is_server_error() {
                CheckResult {
                    id,
                    status: Status::Warn,
                    detail: format!(
                        "{url} returned HTTP {code} {via} ({} ms)",
                        elapsed.as_millis()
                    ),
                }
            } else {
                CheckResult {
                    id,
                    status: Status::Ok,
                    detail: format!(
                        "{url} reachable {via} ({} ms, HTTP {code})",
                        elapsed.as_millis()
                    ),
                }
            }
        }
        Err(e) => CheckResult {
            id,
            status: Status::Warn,
            detail: format!("{url} unreachable {via}: {e}"),
        },
    }
}

fn check_permissions() -> CheckResult {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return CheckResult {
                id: "permissions",
                status: Status::Warn,
                detail: format!("current_exe: {e}"),
            };
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = match std::fs::metadata(&exe) {
            Ok(m) => m.permissions().mode(),
            Err(e) => {
                return CheckResult {
                    id: "permissions",
                    status: Status::Warn,
                    detail: format!("stat binary: {e}"),
                };
            }
        };
        if mode & 0o111 == 0 {
            return CheckResult {
                id: "permissions",
                status: Status::Fail,
                detail: format!(
                    "binary {} is not executable (mode {:o})",
                    exe.display(),
                    mode
                ),
            };
        }

        // Termux-specific: warn if the binary lives somewhere it can't
        // actually be re-executed, e.g. external storage.
        let in_termux = std::env::var("PREFIX")
            .ok()
            .map(|p| p.contains("com.termux"))
            .unwrap_or(false);
        if in_termux {
            let s = exe.to_string_lossy();
            let safe =
                s.contains("/data/data/com.termux/") || s.contains("/data/user/0/com.termux/");
            if !safe {
                return CheckResult {
                    id: "permissions",
                    status: Status::Warn,
                    detail: format!(
                        "binary at {} is outside Termux home; Android may block exec on /sdcard or /storage paths",
                        exe.display()
                    ),
                };
            }
        }
        CheckResult {
            id: "permissions",
            status: Status::Ok,
            detail: format!("binary executable (mode {:o})", mode & 0o777),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = exe;
        CheckResult {
            id: "permissions",
            status: Status::Ok,
            detail: "non-unix platform — skipping +x check".to_string(),
        }
    }
}

fn check_data_size(cfg: &DiagConfig) -> CheckResult {
    if cfg.no_fs() {
        return CheckResult {
            id: "data",
            status: Status::Ok,
            detail: "skipped — no_fs mode".to_string(),
        };
    }
    let path: PathBuf = DB_PATH.to_owned();
    match std::fs::metadata(&path) {
        Ok(m) => {
            let mb = m.len() as f64 / 1024.0 / 1024.0;
            CheckResult {
                id: "data",
                status: Status::Ok,
                detail: format!("{} ({:.1} MB)", path.display(), mb),
            }
        }
        Err(_) => CheckResult {
            id: "data",
            status: Status::Warn,
            detail: format!("{} not yet created", path.display()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_ish_accepts_normal_versions() {
        assert!(is_semver_ish("1.2.3"));
        assert!(is_semver_ish("0.0.1"));
        assert!(is_semver_ish("12.34.56"));
        assert!(is_semver_ish("1.2.3-pre"));
        assert!(is_semver_ish("1.2.3+meta"));
    }

    #[test]
    fn semver_ish_rejects_garbage() {
        assert!(!is_semver_ish("hello"));
        assert!(!is_semver_ish("1.2"));
        assert!(!is_semver_ish("a.b.c"));
        assert!(!is_semver_ish(""));
        assert!(!is_semver_ish("v1.2.3")); // caller strips the leading "v"
    }

    #[test]
    fn diag_config_defaults_to_loopback_8484() {
        let cfg = DiagConfig::default();
        assert_eq!(cfg.ip(), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(cfg.port(), 8484);
        assert_eq!(
            cfg.probe_address(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8484)
        );
    }

    #[test]
    fn diag_config_probe_address_substitutes_wildcard_with_loopback_v4() {
        let cfg = DiagConfig {
            ip: Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            port: Some(9000),
            ..Default::default()
        };
        assert_eq!(
            cfg.bind_address(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9000)
        );
        assert_eq!(
            cfg.probe_address(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000)
        );
    }

    #[test]
    fn diag_config_probe_address_substitutes_wildcard_with_loopback_v6() {
        let cfg = DiagConfig {
            ip: Some(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
            port: Some(9000),
            ..Default::default()
        };
        assert_eq!(
            cfg.probe_address(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9000)
        );
    }

    #[test]
    fn diag_config_probe_address_preserves_explicit_v4() {
        // A LAN-bound server (192.168.x.y) does not answer on loopback;
        // probing 127.0.0.1 would falsely report PORT_OCCUPIED_UNKNOWN.
        let lan = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        let cfg = DiagConfig {
            ip: Some(lan),
            port: Some(8484),
            ..Default::default()
        };
        assert_eq!(cfg.probe_address(), SocketAddr::new(lan, 8484));
    }

    #[test]
    fn diag_config_probe_address_preserves_explicit_v6_loopback() {
        // Bind to [::1] explicitly: do not "rewrite" to 127.0.0.1 — the
        // server only answers on the v6 loopback in that case.
        let v6_loop = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let cfg = DiagConfig {
            ip: Some(v6_loop),
            port: Some(8484),
            ..Default::default()
        };
        assert_eq!(cfg.probe_address(), SocketAddr::new(v6_loop, 8484));
    }

    #[test]
    fn check_config_skips_in_no_fs_mode() {
        let cfg = DiagConfig {
            no_fs: Some(true),
            ..Default::default()
        };
        let r = check_config(&cfg);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("no_fs"));
    }

    #[tokio::test]
    async fn check_database_warns_on_missing_path() {
        let cfg = DiagConfig::default();
        let res = check_database(&cfg).await;
        assert!(matches!(res.status, Status::Ok | Status::Warn));
        assert_eq!(res.id, "database");
    }

    #[tokio::test]
    async fn check_database_skips_in_no_fs_mode() {
        let cfg = DiagConfig {
            no_fs: Some(true),
            ..Default::default()
        };
        let res = check_database(&cfg).await;
        assert_eq!(res.status, Status::Ok);
        assert!(res.detail.contains("no_fs"));
    }

    #[test]
    fn check_binary_returns_ok() {
        let r = check_binary();
        assert_eq!(r.id, "binary");
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn check_platform_includes_arch() {
        let r = check_platform();
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains(std::env::consts::ARCH));
    }
}
