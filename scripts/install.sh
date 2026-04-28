#!/usr/bin/env bash
# scripts/install.sh — fetch the latest clewdr release, install the binary,
# and (by default) hand off to `clewdr service install` to register
# autostart. Pure bash; the network + filesystem + platform detection
# half lives here, every privileged data-touching step happens inside
# the binary so the two paths stay in lockstep.
#
# Usage:
#   curl -fL https://raw.githubusercontent.com/<owner>/<repo>/master/scripts/install.sh | bash
#   bash install.sh
#
# Environment overrides (all optional):
#   CLEWDR_REPO_OWNER     repo owner; default: waylon256yhw
#   CLEWDR_REPO_NAME      repo name;  default: clewdr-hub
#   CLEWDR_RELEASE_TAG    release tag (e.g. v1.2.4) or "latest" (default).
#   CLEWDR_INSTALL_DIR    override install dir (default: /opt/clewdr on
#                         Linux/macOS, $HOME/clewdr on Termux).
#   CLEWDR_BIN_LINK       override PATH symlink (default: /usr/local/bin/clewdr
#                         on Linux/macOS, $PREFIX/bin/clewdr on Termux).
#   CLEWDR_NO_SERVICE=1   don't run `clewdr service install` at the end.
#                         Operator can run it later by hand.
#
# Exit codes: 0 success; 1 hard failure (network / extraction / perms);
# 2 unsupported platform.

set -euo pipefail

# ──────────────────────────────────────────────────────────────────────
# config / constants
# ──────────────────────────────────────────────────────────────────────

REPO_OWNER="${CLEWDR_REPO_OWNER:-waylon256yhw}"
REPO_NAME="${CLEWDR_REPO_NAME:-clewdr-hub}"
RELEASE_TAG="${CLEWDR_RELEASE_TAG:-latest}"
INSTALL_DIR_OVERRIDE="${CLEWDR_INSTALL_DIR:-}"
NO_SERVICE="${CLEWDR_NO_SERVICE:-0}"
BIN_LINK_PATH="${CLEWDR_BIN_LINK:-}"

# Minimum host glibc that can run the GitHub `linux-*` build artifacts.
# CI builds them on ubuntu-latest (glibc 2.39 at the time of writing);
# anything older falls back to the static musllinux-* asset to avoid
# `GLIBC_X.Y not found` at first invocation. Bump this in lockstep
# with .github/workflows/build.yml runner upgrades. Operators on
# unusual hosts (e.g. compatibility libcs) can override.
REQUIRED_GLIBC="${CLEWDR_REQUIRED_GLIBC:-2.36}"

# Color helpers; auto-disable when stdout isn't a TTY (curl|bash piping
# to something that captures stdout).
if [[ -t 1 ]]; then
    RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'
    BOLD=$'\033[1m'; RESET=$'\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BOLD=''; RESET=''
fi
err()  { echo "${RED}${BOLD}error:${RESET} $*" >&2; exit 1; }
info() { echo "${GREEN}${BOLD}==>${RESET} $*"; }
warn() { echo "${YELLOW}${BOLD}warn:${RESET} $*" >&2; }

require() {
    command -v "$1" >/dev/null 2>&1 || err "missing required tool: $1 (install it first, then re-run)"
}

# Pure comparison: returns 0 iff $1 >= $2 by MAJOR.MINOR. The minor
# component compares as a plain integer (so 2.31 < 2.36 < 2.100), which
# is what semver-like glibc release numbers want. An empty $1 (no
# detected version) returns non-zero so the caller's musl fallback
# kicks in — better safe than handing a broken dynamic binary to a
# host whose libc we can't read.
version_at_least() {
    local have="$1" need="$2"
    [[ -n "$have" ]] || return 1
    local have_major=${have%%.*} need_major=${need%%.*}
    local have_minor=${have#*.} need_minor=${need#*.}
    have_minor=${have_minor%%.*}
    need_minor=${need_minor%%.*}
    if   (( have_major > need_major )); then return 0
    elif (( have_major < need_major )); then return 1
    else                                     (( have_minor >= need_minor ))
    fi
}

# Best-effort glibc version probe. `getconf GNU_LIBC_VERSION` is the
# preferred source; falls back to parsing `ldd --version`'s first line.
# Echoes a MAJOR.MINOR string, or empty if neither source produced one.
#
# Both lookups are guarded by `command -v` AND `|| true` because the
# script runs under `set -euo pipefail`: if `getconf` is missing (127)
# the pipeline's pipefail status would propagate through `$(...)` and
# kill the script before the ldd fallback ever ran.
host_glibc_version() {
    local v=""
    if command -v getconf >/dev/null 2>&1; then
        v=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}') || v=""
    fi
    if [[ -z "$v" ]] && command -v ldd >/dev/null 2>&1; then
        v=$(ldd --version 2>&1 | head -1 | grep -oE '[0-9]+\.[0-9]+' | head -1) || v=""
    fi
    echo "$v"
}

# Returns 0 iff the host glibc is >= $1. Wrapper that composes
# host_glibc_version + version_at_least so the pure comparison is
# unit-testable without process mocks.
glibc_at_least() {
    version_at_least "$(host_glibc_version)" "$1"
}

# Picks `linux` vs `musllinux` for a glibc-detected host (the musl
# branch in detect_target() short-circuits before this is called).
# Pure — covered by the helper smoke tests; bumping REQUIRED_GLIBC
# changes which branch a given host lands in.
pick_linux_libc() {
    if glibc_at_least "$REQUIRED_GLIBC"; then
        echo "linux"
    else
        echo "musllinux"
    fi
}

# ──────────────────────────────────────────────────────────────────────
# platform detection
# ──────────────────────────────────────────────────────────────────────

# Returns "<os>-<arch>" matching the asset names produced by
# .github/workflows/build.yml: `clewdr-{os}-{arch}.zip`.
detect_target() {
    local uname_s uname_m os arch
    uname_s=$(uname -s)
    uname_m=$(uname -m)

    # Termux check first — Termux on Android reports `uname -s`=Linux
    # but exposes the com.termux PREFIX env var. The build matrix names
    # this target "android".
    if [[ -n "${PREFIX:-}" && "$PREFIX" == *com.termux* ]] \
       || [[ -d /data/data/com.termux/files/usr ]]; then
        os="android"
    else
        case "$uname_s" in
            Linux)
                # libc detection. ldd --version reports "musl libc" on
                # musl, glibc otherwise. On glibc systems we ALSO need
                # to check the version — the `linux-*` build artifacts
                # are built on ubuntu-latest (currently glibc 2.39) and
                # won't run on hosts older than REQUIRED_GLIBC. Older
                # glibc hosts get routed to the static musllinux asset.
                if ldd --version 2>&1 | grep -qiE 'musl'; then
                    os="musllinux"
                else
                    os=$(pick_linux_libc)
                    if [[ "$os" == "musllinux" ]]; then
                        warn "host glibc looks older than ${REQUIRED_GLIBC}; falling back to static musllinux build"
                    fi
                fi
                ;;
            Darwin) os="macos" ;;
            *)      err "unsupported OS: $uname_s (this script supports Linux, macOS, Termux/Android)" ;;
        esac
    fi

    case "$uname_m" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) err "unsupported architecture: $uname_m" ;;
    esac

    # Android only ships aarch64 in the build matrix; x86_64 is
    # excluded there per .github/workflows/build.yml.
    if [[ "$os" == "android" && "$arch" != "aarch64" ]]; then
        err "android build only ships aarch64; this device reports $arch"
    fi

    echo "${os}-${arch}"
}

# Decide the install dir. Termux installs are user-local because the
# user has no privileged path — and Android refuses chmod +x under
# /sdcard, so anywhere in $HOME is the only option (plan §8 risk #8).
# Linux/macOS go to /opt/clewdr to match the systemd unit's
# WorkingDirectory.
resolve_install_dir() {
    local target=$1
    if [[ -n "$INSTALL_DIR_OVERRIDE" ]]; then
        echo "$INSTALL_DIR_OVERRIDE"
        return
    fi
    case "$target" in
        android-*) echo "${HOME}/clewdr" ;;
        *)         echo "/opt/clewdr" ;;
    esac
}

default_bin_link_path() {
    local target=$1
    case "$target" in
        android-*) echo "${PREFIX:-/data/data/com.termux/files/usr}/bin/clewdr" ;;
        *)         echo "/usr/local/bin/clewdr" ;;
    esac
}

# Returns 0 iff `clewdr service install` has a backend on this host
# AND the caller has the privileges the verb needs. The verb supports
# only systemd (Linux, requires root) and Termux:Boot (Android, no
# root needed). macOS, BSDs, stripped containers without systemd, and
# non-root users on systemd hosts all need to be skipped — otherwise
# the verb errors after the binary is already in place and `set -e`
# aborts the rest of the script.
service_supported() {
    local target=$1
    case "$target" in
        android-*)
            # Termux:Boot doesn't need root.
            return 0
            ;;
        linux-*|musllinux-*)
            # systemd backend AND root.
            [[ -d /run/systemd/system ]] || return 1
            (( EUID == 0 )) || return 1
            return 0
            ;;
        *)
            # macOS / windows / unknown.
            return 1
            ;;
    esac
}

install_path_link() {
    local target=$1 install_dir=$2 bin_name=$3

    case "$target" in
        android-*|linux-*|musllinux-*|macos-*) ;;
        *) return 0 ;;
    esac

    if [[ "$BIN_LINK_PATH" == /opt/* || "$BIN_LINK_PATH" == /usr/* ]] && [[ $EUID -ne 0 ]]; then
        warn "skipping PATH link at ${BIN_LINK_PATH}; creating it requires root"
        return 0
    fi

    mkdir -p "$(dirname "$BIN_LINK_PATH")"
    if [[ -e "$BIN_LINK_PATH" || -L "$BIN_LINK_PATH" ]]; then
        local existing=""
        existing=$(readlink "$BIN_LINK_PATH" 2>/dev/null || true)
        if [[ "$existing" != "${install_dir}/${bin_name}" ]]; then
            warn "skipping PATH link; ${BIN_LINK_PATH} already exists"
            warn "run directly with: ${install_dir}/${bin_name}"
            return 0
        fi
    fi

    ln -sfn "${install_dir}/${bin_name}" "$BIN_LINK_PATH"
    info "PATH link: ${BOLD}${BIN_LINK_PATH}${RESET} -> ${install_dir}/${bin_name}"
}

clewdr_on_path_points_to_install() {
    local install_dir=$1 bin_name=$2 found="" link_target=""
    found=$(command -v clewdr 2>/dev/null || true)
    [[ "$found" == /* ]] || return 1
    [[ "$found" == "${install_dir}/${bin_name}" ]] && return 0
    if [[ -L "$found" ]]; then
        link_target=$(readlink "$found" 2>/dev/null || true)
        [[ "$link_target" == "${install_dir}/${bin_name}" ]] && return 0
    fi
    return 1
}

# ──────────────────────────────────────────────────────────────────────
# release lookup
# ──────────────────────────────────────────────────────────────────────

# Builds the GitHub releases API URL for the selected tag.
release_api_url() {
    if [[ "$RELEASE_TAG" == "latest" ]]; then
        echo "https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}/releases/latest"
    else
        echo "https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}/releases/tags/${RELEASE_TAG}"
    fi
}

# Pull the matching asset URL out of a GitHub release JSON. Pure stdin
# transformer — kept separate so it's easy to feed a fixture in tests.
# Returns empty (and exits 0) when no asset matches; the caller checks
# for empty and emits a friendlier error than `grep | head | sed`'s
# `set -e` abort on a missing pattern.
extract_asset_url() {
    local target=$1 asset_name
    asset_name="clewdr-${target}.zip"
    {
        grep -oE "\"browser_download_url\"[[:space:]]*:[[:space:]]*\"[^\"]*${asset_name}\"" \
            | head -1 \
            | sed -E 's/.*"(https[^"]+)".*/\1/'
    } || true
}

# ──────────────────────────────────────────────────────────────────────
# main
# ──────────────────────────────────────────────────────────────────────

require curl
require unzip

target=$(detect_target)
info "platform: ${BOLD}${target}${RESET}"

if [[ -z "$BIN_LINK_PATH" ]]; then
    BIN_LINK_PATH=$(default_bin_link_path "$target")
fi

install_dir=$(resolve_install_dir "$target")
info "install dir: ${BOLD}${install_dir}${RESET}"

api_url=$(release_api_url)
info "fetching release metadata from ${api_url}"
release_json=$(curl -fsSL "$api_url") || err "failed to fetch release metadata (network or rate-limit?)"

download_url=$(echo "$release_json" | extract_asset_url "$target")
[[ -n "$download_url" ]] \
    || err "no asset matching clewdr-${target}.zip in release ${RELEASE_TAG} (mismatched build matrix?)"
info "asset: $download_url"

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

info "downloading..."
curl -fsSL "$download_url" -o "$tmpdir/clewdr.zip"

info "extracting..."
mkdir -p "$tmpdir/extracted"
unzip -q "$tmpdir/clewdr.zip" -d "$tmpdir/extracted"

bin_name="clewdr"
[[ "$target" == windows-* ]] && bin_name="clewdr.exe"
[[ -f "$tmpdir/extracted/$bin_name" ]] \
    || err "extracted archive doesn't contain $bin_name (asset corrupted?)"

# /opt and /usr need root. The systemd path will need root anyway for
# `service install`, so failing here with a clear message is friendlier
# than discovering it three steps later.
if [[ "$install_dir" == /opt/* || "$install_dir" == /usr/* ]] && [[ $EUID -ne 0 ]]; then
    err "writing to ${install_dir} requires root — re-run with sudo, or set CLEWDR_INSTALL_DIR=\$HOME/clewdr for a user-local install"
fi

mkdir -p "$install_dir"
mv "$tmpdir/extracted/$bin_name" "$install_dir/$bin_name"
chmod +x "$install_dir/$bin_name"

# Android NDK builds ship libc++_shared.so alongside the binary; it
# must end up in the same dir or the dynamic loader can't find it.
if [[ "$target" == android-* ]] && [[ -f "$tmpdir/extracted/libc++_shared.so" ]]; then
    mv "$tmpdir/extracted/libc++_shared.so" "$install_dir/"
fi

info "installed: ${BOLD}${install_dir}/${bin_name}${RESET}"
install_path_link "$target" "$install_dir" "$bin_name"

# Service registration. The binary handles env detection (systemd vs
# Termux:Boot) and the privileged setup (useradd, /etc/systemd/, etc.),
# so this script doesn't need any per-OS branching here — but we DO
# need to skip the call entirely when the host has no supported
# backend (macOS, sysvinit Linux, stripped containers). Otherwise the
# verb errors out and `set -e` aborts the script after the binary's
# already in place.
if [[ "$NO_SERVICE" == "1" ]]; then
    warn "skipping service registration (CLEWDR_NO_SERVICE=1)"
    info "register later with: ${install_dir}/${bin_name} service install"
elif ! service_supported "$target"; then
    warn "no supported service backend detected (need systemd on Linux or Termux:Boot on Android)"
    info "skipping service registration; the binary is installed but won't autostart on reboot"
    info "manual start: ${install_dir}/${bin_name} serve"
    info "register later (if you set up systemd / Termux:Boot): ${install_dir}/${bin_name} service install"
else
    info "registering service..."
    "$install_dir/$bin_name" service install
fi

info "${GREEN}${BOLD}install complete${RESET}"
if clewdr_on_path_points_to_install "$install_dir" "$bin_name"; then
    info "diagnose:  clewdr diagnose"
    info "menu:      clewdr menu"
else
    info "diagnose:  ${install_dir}/${bin_name} diagnose"
    info "menu:      ${install_dir}/${bin_name} menu"
fi
