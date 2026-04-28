//! `clewdr status` — running state, port, version, service registration.
//!
//! Authoritative source for "is it running" is the HTTP probe to
//! `/api/version` on the configured listener address. Service-layer
//! detection (systemd / Termux:Boot) is layered on top so we can
//! distinguish "service active but not yet answering" from "stopped" and
//! report the right next step to the operator.
//!
//! State precedence:
//! 1. HTTP `GET /api/version` succeeds + body matches `v\d+\.\d+\.\d+...`
//!    → `Running` (with live-vs-binary version match)
//! 2. HTTP succeeds + body is foreign → `PortOccupiedOther`
//! 3. HTTP fails + service active → `StartingOrBroken`
//! 4. HTTP fails + service inactive / not registered → `Stopped`
//!
//! Reads clewdr.toml via Figment (not via `CLEWDR_CONFIG`) to keep the
//! verb side-effect-free.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use colored::Colorize;
use serde::{Deserialize, Serialize};

use crate::{
    config::{CONFIG_PATH, DB_PATH},
    error::ClewdrError,
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const SYSTEMD_UNIT: &str = "clewdr.service";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Running,
    StartingOrBroken,
    Stopped,
    PortOccupiedOther,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ServiceInfo {
    Systemd {
        unit: String,
        active_state: String,
        active: bool,
    },
    TermuxBoot {
        script: String,
        present: bool,
    },
    NotRegistered,
}

#[derive(Debug, Clone, Serialize)]
struct StatusReport {
    binary: String,
    binary_version: String,
    state: State,
    bind_address: String,
    probe_address: String,
    live_version: Option<String>,
    version_match: Option<bool>,
    /// HTTP status code returned by the local probe when something other
    /// than clewdr was holding the port. `None` for the running / refused /
    /// timeout cases.
    probe_http_status: Option<u16>,
    /// Last-resort debug string when the probe failed for a reason we
    /// couldn't classify (TLS issue, DNS, etc.). `None` for clean states.
    probe_error: Option<String>,
    pid: Option<u32>,
    service: ServiceInfo,
    db_path: String,
    db_size_bytes: Option<u64>,
    no_fs: bool,
}

pub async fn run(args: Args) -> Result<(), ClewdrError> {
    let report = build_report().await;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print_human(&report);
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Report assembly
// ──────────────────────────────────────────────────────────────────────────

async fn build_report() -> StatusReport {
    let cfg = read_minimal_config();
    let bind = cfg.bind_address();
    let probe = cfg.probe_address();
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let binary_version = format!("v{}", env!("CARGO_PKG_VERSION"));

    let service = detect_service().await;
    let probe_outcome = http_probe(probe).await;
    let state = compute_state(&probe_outcome, &service);
    let pid = pid_for_listener(&cfg);

    let live_version = match &probe_outcome {
        ProbeOutcome::Clewdr { version } => Some(version.clone()),
        _ => None,
    };
    let version_match = live_version.as_deref().map(|live| live == binary_version);
    let probe_http_status = match &probe_outcome {
        ProbeOutcome::ForeignHttp { status } => Some(*status),
        _ => None,
    };
    let probe_error = match &probe_outcome {
        ProbeOutcome::Error { msg } => Some(msg.clone()),
        _ => None,
    };

    let db_path = DB_PATH.to_owned();
    let db_size_bytes = if cfg.no_fs() {
        None
    } else {
        std::fs::metadata(&db_path).ok().map(|m| m.len())
    };

    StatusReport {
        binary,
        binary_version,
        state,
        bind_address: bind.to_string(),
        probe_address: probe.to_string(),
        live_version,
        version_match,
        probe_http_status,
        probe_error,
        pid,
        service,
        db_path: db_path.display().to_string(),
        db_size_bytes,
        no_fs: cfg.no_fs(),
    }
}

fn compute_state(probe: &ProbeOutcome, service: &ServiceInfo) -> State {
    match probe {
        ProbeOutcome::Clewdr { .. } => State::Running,
        ProbeOutcome::ForeignHttp { .. } => State::PortOccupiedOther,
        ProbeOutcome::Refused | ProbeOutcome::Unreachable => match service {
            ServiceInfo::Systemd { active: true, .. } => State::StartingOrBroken,
            ServiceInfo::Systemd { active: false, .. } => State::Stopped,
            ServiceInfo::TermuxBoot { present: true, .. } => State::Stopped,
            ServiceInfo::TermuxBoot { present: false, .. } => State::Stopped,
            ServiceInfo::NotRegistered => State::Stopped,
        },
        ProbeOutcome::Error { .. } => State::Unknown,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Config (Figment, no CLEWDR_CONFIG side effects)
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct StatusConfig {
    ip: Option<IpAddr>,
    port: Option<u16>,
    no_fs: Option<bool>,
}

impl StatusConfig {
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
    /// Substitute loopback only for wildcard binds; preserve explicit IPs
    /// (mirrors the diagnose verb's logic so we don't false-negative a
    /// LAN-bound or `[::1]`-bound server).
    fn probe_address(&self) -> SocketAddr {
        let probe_ip = match self.ip() {
            IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            other => other,
        };
        SocketAddr::new(probe_ip, self.port())
    }
}

fn read_minimal_config() -> StatusConfig {
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
// HTTP probe — same shape as diagnose, but classifies into outcomes the
// state machine consumes.
// ──────────────────────────────────────────────────────────────────────────

enum ProbeOutcome {
    /// 200 + body matches a clewdr version string.
    Clewdr { version: String },
    /// 200 + body that is *not* a clewdr version (some other HTTP server is
    /// holding the port).
    ForeignHttp { status: u16 },
    /// TCP connect refused — nothing listening at all.
    Refused,
    /// TCP connected but the request didn't complete (timeout, partial,
    /// reset). Treated like Refused for state purposes.
    Unreachable,
    /// HTTP client itself failed to construct (extremely rare).
    Error { msg: String },
}

async fn http_probe(addr: SocketAddr) -> ProbeOutcome {
    let url = format!("http://{addr}/api/version");
    let client = match wreq::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return ProbeOutcome::Error { msg: e.to_string() },
    };
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            // Heuristic: if the error string mentions "refused" treat as a
            // clean Refused; otherwise it's "Unreachable" (timeout, reset,
            // TLS, etc.). We don't currently have a structured kind on the
            // wreq error.
            let msg = e.to_string().to_lowercase();
            if msg.contains("refused") {
                return ProbeOutcome::Refused;
            }
            return ProbeOutcome::Unreachable;
        }
    };
    // Capture the *actual* status code up front so a 201/204/etc. body
    // that happens to fall through to the foreign-shape branch below
    // doesn't get reported as 200.
    let status_code = resp.status().as_u16();
    if !resp.status().is_success() {
        return ProbeOutcome::ForeignHttp {
            status: status_code,
        };
    }
    let body = resp.text().await.unwrap_or_default();
    let trimmed = body.trim();
    if trimmed.starts_with('v') && is_semver_ish(&trimmed[1..]) {
        ProbeOutcome::Clewdr {
            version: trimmed.to_string(),
        }
    } else {
        ProbeOutcome::ForeignHttp {
            status: status_code,
        }
    }
}

fn is_semver_ish(s: &str) -> bool {
    let mut parts = s.split('.');
    let (a, b, c) = (parts.next(), parts.next(), parts.next());
    match (a, b, c) {
        (Some(x), Some(y), Some(z)) => {
            let z_num: String = z.chars().take_while(|c| c.is_ascii_digit()).collect();
            x.chars().all(|c| c.is_ascii_digit())
                && y.chars().all(|c| c.is_ascii_digit())
                && !z_num.is_empty()
        }
        _ => false,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Service detection — systemd, Termux:Boot, or none
// ──────────────────────────────────────────────────────────────────────────

async fn detect_service() -> ServiceInfo {
    if is_termux() {
        return detect_termux_boot();
    }
    if Path::new("/run/systemd/system").exists() {
        if let Some(info) = detect_systemd().await {
            return info;
        }
    }
    ServiceInfo::NotRegistered
}

fn is_termux() -> bool {
    std::env::var("PREFIX")
        .ok()
        .map(|p| p.contains("com.termux"))
        .unwrap_or(false)
        || Path::new("/data/data/com.termux").exists()
}

fn detect_termux_boot() -> ServiceInfo {
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data/data/com.termux/files/home"));
    let script = home.join(".termux/boot/clewdr-hub");
    let present = script.exists();
    ServiceInfo::TermuxBoot {
        script: script.display().to_string(),
        present,
    }
}

async fn detect_systemd() -> Option<ServiceInfo> {
    // `systemctl show <unit> --property=LoadState,ActiveState --value` gives
    // us two definitive answers in one call:
    //   LoadState   = "loaded" / "not-found" / "masked" / "error"
    //   ActiveState = "active" / "inactive" / "failed" / "activating" / ...
    //
    // This is more discriminating than `is-active`:
    //   * `is-active` returns "inactive" exit 3 for *both* a missing unit
    //     and an installed-but-stopped unit, so we couldn't distinguish
    //     "register me" from "start me".
    //   * On hosts where systemd is present but the user can't reach the
    //     bus (containers, chroots), `show` exits non-zero with empty
    //     stdout — we treat that as "no info" and fall back to
    //     NotRegistered instead of synthesising a bogus
    //     `Systemd { active_state: "" }` row.
    //
    // Uses std::process::Command (sync) rather than tokio::process to
    // avoid pulling in tokio's `process` feature; the call returns in
    // under 10ms on every system we care about.
    let out = std::process::Command::new("systemctl")
        .args([
            "show",
            SYSTEMD_UNIT,
            "--property=LoadState,ActiveState",
            "--value",
            "--no-pager",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let load_state = lines.next()?.trim().to_string();
    let active_state = lines.next()?.trim().to_string();

    if load_state != "loaded" {
        // not-found / masked / error — the unit isn't usable as a clewdr
        // service registration even if systemd itself is healthy.
        return None;
    }

    // ActiveState values that count as "really running":
    let active = matches!(active_state.as_str(), "active" | "reloading");
    Some(ServiceInfo::Systemd {
        unit: SYSTEMD_UNIT.to_string(),
        active_state,
        active,
    })
}

// ──────────────────────────────────────────────────────────────────────────
// PID lookup — best-effort, /proc-only (Linux + Android/Termux)
// ──────────────────────────────────────────────────────────────────────────

#[cfg(any(target_os = "linux", target_os = "android"))]
fn pid_for_listener(cfg: &StatusConfig) -> Option<u32> {
    proc_pid_for_listener(cfg.ip(), cfg.port())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn pid_for_listener(_cfg: &StatusConfig) -> Option<u32> {
    None
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn proc_pid_for_listener(want_ip: IpAddr, want_port: u16) -> Option<u32> {
    use std::{
        fs,
        io::{BufRead, BufReader},
    };

    // Step 1: pick the right /proc/net table for the configured family,
    // then collect socket inodes that are LISTEN-ing on want_ip:want_port.
    //
    // Filtering by *both* address and port is load-bearing: if clewdr is
    // bound to [::1]:8484 and an unrelated process is bound to
    // 127.0.0.1:8484, looking at port alone would let us return the wrong
    // PID even though the HTTP probe correctly identified ours.
    let table = match want_ip {
        IpAddr::V4(_) => "/proc/net/tcp",
        IpAddr::V6(_) => "/proc/net/tcp6",
    };

    let mut inodes: Vec<u64> = Vec::new();
    if let Ok(f) = fs::File::open(table) {
        for line in BufReader::new(f).lines().map_while(Result::ok).skip(1) {
            // sl  local_address rem_address st tx_q rx_q tr tm->when retrnsmt   uid timeout inode
            //  0  CD000000:21A4 ...        0A   ...                                         12345
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                continue; // not LISTEN
            }
            let local = fields[1];
            let Some((addr_hex, port_hex)) = local.rsplit_once(':') else {
                continue;
            };
            let Ok(p) = u16::from_str_radix(port_hex, 16) else {
                continue;
            };
            if p != want_port {
                continue;
            }
            let Some(local_ip) = parse_proc_net_addr(addr_hex) else {
                continue;
            };
            if local_ip != want_ip {
                continue;
            }
            if let Ok(inode) = fields[9].parse::<u64>() {
                inodes.push(inode);
            }
        }
    }
    if inodes.is_empty() {
        return None;
    }

    // Step 2: walk /proc/<pid>/fd looking for `socket:[<inode>]` symlinks.
    // Permissions: an unprivileged caller can only see their own fds, so
    // this returns None when checking on a server that runs as a different
    // user (common with the systemd unit). That's acceptable — `pid` is a
    // best-effort field.
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let s = target.to_string_lossy();
            if let Some(rest) = s.strip_prefix("socket:[")
                && let Some(num) = rest.strip_suffix(']')
                && let Ok(inode) = num.parse::<u64>()
                && inodes.contains(&inode)
            {
                return Some(pid);
            }
        }
    }
    None
}

/// Parse the `local_address` hex column from `/proc/net/tcp{,6}`.
///
/// The kernel writes each 32-bit word in *host* byte order, hex-encoded.
/// On the little-endian platforms we ship for (x86_64, aarch64, armv7),
/// that means each 4-byte word is reversed relative to network order:
///
///   * IPv4: `"0100007F"` → `7F.00.00.01` → 127.0.0.1
///   * IPv6: `"00000000000000000000000001000000"` → `::1`
///
/// On big-endian hosts we'd need `to_be_bytes` instead, but Linux/Android
/// production targets are LE; we cfg on `target_endian` to keep the door
/// open.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn parse_proc_net_addr(hex: &str) -> Option<IpAddr> {
    fn word_bytes(word: u32) -> [u8; 4] {
        if cfg!(target_endian = "little") {
            word.to_le_bytes()
        } else {
            word.to_be_bytes()
        }
    }

    match hex.len() {
        8 => {
            let word = u32::from_str_radix(hex, 16).ok()?;
            Some(IpAddr::V4(Ipv4Addr::from(word_bytes(word))))
        }
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                let word = u32::from_str_radix(&hex[i * 8..(i + 1) * 8], 16).ok()?;
                bytes[i * 4..(i + 1) * 4].copy_from_slice(&word_bytes(word));
            }
            Some(IpAddr::V6(Ipv6Addr::from(bytes)))
        }
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Output
// ──────────────────────────────────────────────────────────────────────────

fn print_human(r: &StatusReport) {
    println!("{}", "clewdr status".bold());
    line("binary", &format!("{} ({})", r.binary, r.binary_version));
    line("state", &state_line(r));
    line("bind", &r.bind_address);
    if r.bind_address != r.probe_address {
        line(
            "probe",
            &format!("{} (loopback substitute)", r.probe_address),
        );
    }
    if let Some(pid) = r.pid {
        line("pid", &pid.to_string());
    } else if r.state == State::Running {
        line("pid", "(unknown — different uid or unsupported platform)");
    }
    line("service", &service_line(&r.service));
    if r.no_fs {
        line("data", "in-memory (no_fs mode)");
    } else {
        match r.db_size_bytes {
            Some(n) => {
                let mb = n as f64 / 1024.0 / 1024.0;
                line("data", &format!("{} ({:.1} MB)", r.db_path, mb));
            }
            None => line("data", &format!("{} (not yet created)", r.db_path)),
        }
    }
}

fn line(label: &str, value: &str) {
    println!("  {:<8} {}", format!("{label}:"), value);
}

fn state_line(r: &StatusReport) -> String {
    match r.state {
        State::Running => match (r.live_version.as_deref(), r.version_match) {
            (Some(live), Some(true)) => {
                format!(
                    "{} — live={live} (matches binary)",
                    "RUNNING".green().bold()
                )
            }
            (Some(live), Some(false)) => {
                format!(
                    "{} — live={live}, binary={} ({})",
                    "RUNNING".yellow().bold(),
                    r.binary_version,
                    "restart needed to pick up the new binary".yellow()
                )
            }
            _ => format!("{}", "RUNNING".green().bold()),
        },
        State::StartingOrBroken => format!(
            "{} — service active but {} not answering. Check `journalctl -u clewdr` for the cause.",
            "STARTING_OR_BROKEN".yellow().bold(),
            r.probe_address
        ),
        State::Stopped => format!("{}", "STOPPED".red().bold()),
        State::PortOccupiedOther => {
            let extra = match r.probe_http_status {
                Some(c) => format!(" (HTTP {c})"),
                None => String::new(),
            };
            format!(
                "{} — {}{} responded but isn't a clewdr server",
                "PORT_OCCUPIED_OTHER".yellow().bold(),
                r.probe_address,
                extra
            )
        }
        State::Unknown => {
            let extra = match &r.probe_error {
                Some(e) => format!(" ({e})"),
                None => String::new(),
            };
            format!("{}{}", "UNKNOWN".yellow().bold(), extra)
        }
    }
}

fn service_line(s: &ServiceInfo) -> String {
    match s {
        ServiceInfo::Systemd {
            unit,
            active_state,
            active,
        } => {
            let tag = if *active {
                active_state.green().to_string()
            } else {
                active_state.yellow().to_string()
            };
            format!("systemd: {unit} ({tag})")
        }
        ServiceInfo::TermuxBoot { script, present } => {
            if *present {
                format!("Termux:Boot script at {script} ({})", "present".green())
            } else {
                format!(
                    "Termux:Boot script at {script} ({} — install with `clewdr service install`)",
                    "missing".yellow()
                )
            }
        }
        ServiceInfo::NotRegistered => format!(
            "{} (running ad-hoc; install with `clewdr service install`)",
            "not registered".yellow()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_default() -> StatusConfig {
        StatusConfig::default()
    }

    #[test]
    fn semver_ish_matches_api_version_format() {
        // Mirrors what /api/version returns: `v1.2.3`, with the leading
        // `v` stripped before this helper runs.
        assert!(is_semver_ish("1.2.3"));
        assert!(is_semver_ish("1.2.3-pre"));
        assert!(!is_semver_ish("hello"));
        assert!(!is_semver_ish("1.2"));
    }

    #[test]
    fn probe_address_substitutes_only_wildcards() {
        let mut cfg = cfg_default();
        cfg.ip = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(cfg.probe_address().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

        cfg.ip = Some(IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert_eq!(cfg.probe_address().ip(), IpAddr::V6(Ipv6Addr::LOCALHOST));

        let lan = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        cfg.ip = Some(lan);
        assert_eq!(cfg.probe_address().ip(), lan);

        let v6_loop = IpAddr::V6(Ipv6Addr::LOCALHOST);
        cfg.ip = Some(v6_loop);
        assert_eq!(cfg.probe_address().ip(), v6_loop);
    }

    #[test]
    fn compute_state_clewdr_response_is_running() {
        let probe = ProbeOutcome::Clewdr {
            version: "v1.2.3".into(),
        };
        let svc = ServiceInfo::NotRegistered;
        assert_eq!(compute_state(&probe, &svc), State::Running);
    }

    #[test]
    fn compute_state_foreign_http_is_port_occupied() {
        let probe = ProbeOutcome::ForeignHttp { status: 200 };
        let svc = ServiceInfo::NotRegistered;
        assert_eq!(compute_state(&probe, &svc), State::PortOccupiedOther);
    }

    #[test]
    fn compute_state_refused_with_active_service_is_starting_or_broken() {
        let probe = ProbeOutcome::Refused;
        let svc = ServiceInfo::Systemd {
            unit: "clewdr.service".into(),
            active_state: "active".into(),
            active: true,
        };
        assert_eq!(compute_state(&probe, &svc), State::StartingOrBroken);
    }

    #[test]
    fn compute_state_refused_with_inactive_service_is_stopped() {
        let probe = ProbeOutcome::Refused;
        let svc = ServiceInfo::Systemd {
            unit: "clewdr.service".into(),
            active_state: "inactive".into(),
            active: false,
        };
        assert_eq!(compute_state(&probe, &svc), State::Stopped);
    }

    #[test]
    fn compute_state_refused_without_service_is_stopped() {
        let probe = ProbeOutcome::Refused;
        let svc = ServiceInfo::NotRegistered;
        assert_eq!(compute_state(&probe, &svc), State::Stopped);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn parse_proc_net_addr_decodes_v4() {
        // 127.0.0.1 → "0100007F" (LE-per-word), [::1] → 16-byte form below.
        assert_eq!(
            parse_proc_net_addr("0100007F"),
            Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))
        );
        // 0.0.0.0 wildcard
        assert_eq!(
            parse_proc_net_addr("00000000"),
            Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        );
        // 192.168.1.10 → bytes [192, 168, 1, 10] → LE u32 word "0A01A8C0"
        assert_eq!(
            parse_proc_net_addr("0A01A8C0"),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)))
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn parse_proc_net_addr_decodes_v6() {
        // [::1] is the only non-zero v6 address we care about for tests.
        // The 16 bytes [0,0,...,0,1] become 4 LE words; the last word
        // contains 0x00000001 (network order), reversed → "01000000".
        assert_eq!(
            parse_proc_net_addr("00000000000000000000000001000000"),
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
        // [::] wildcard
        assert_eq!(
            parse_proc_net_addr("00000000000000000000000000000000"),
            Some(IpAddr::V6(Ipv6Addr::UNSPECIFIED))
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn parse_proc_net_addr_rejects_invalid_lengths() {
        assert!(parse_proc_net_addr("").is_none());
        assert!(parse_proc_net_addr("12").is_none());
        assert!(parse_proc_net_addr("nothex!!").is_none());
    }
}
