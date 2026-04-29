#!/usr/bin/env bash
# scripts/uninstall.sh — reverse install.sh.
#
# Default behaviour: call `clewdr service uninstall` (which removes the
# systemd unit / Termux:Boot script) and delete the binary. clewdr.db,
# clewdr.toml, and the log directory are *preserved* — the service
# uninstall verb only deletes them when --purge is passed.
#
# Pass --purge here to delete data too. When a service backend is usable,
# the binary's service verb performs the interactive 'yes' confirmation;
# otherwise this script confirms and deletes the install.sh-managed local
# paths itself.
#
# Usage:
#   bash uninstall.sh [--purge]
#
# Environment overrides:
#   CLEWDR_INSTALL_DIR    install dir to clean up (default: auto-detect
#                         /opt/clewdr or $HOME/clewdr).
#   CLEWDR_BIN_LINK       PATH symlink to clean up if it points to this
#                         install (default: /usr/local/bin/clewdr on
#                         Linux/macOS, $PREFIX/bin/clewdr on Termux).

set -euo pipefail

PURGE=0
INSTALL_DIR_OVERRIDE="${CLEWDR_INSTALL_DIR:-}"
NO_SERVICE="${CLEWDR_NO_SERVICE:-0}"
BIN_LINK_PATH="${CLEWDR_BIN_LINK:-}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --purge)
            PURGE=1
            shift
            ;;
        --no-service)
            # Equivalent to CLEWDR_NO_SERVICE=1; useful when the user
            # installed binary-only (CLEWDR_NO_SERVICE=1) and the host
            # *does* have a service backend (systemd) but no service
            # was ever registered.
            NO_SERVICE=1
            shift
            ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's|^# \{0,1\}||'
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            echo "usage: $0 [--purge] [--no-service]" >&2
            exit 2
            ;;
    esac
done

if [[ -t 1 ]]; then
    RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'
    BOLD=$'\033[1m'; RESET=$'\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BOLD=''; RESET=''
fi
err()  { echo "${RED}${BOLD}error:${RESET} $*" >&2; exit 1; }
info() { echo "${GREEN}${BOLD}==>${RESET} $*"; }
warn() { echo "${YELLOW}${BOLD}warn:${RESET} $*" >&2; }

is_termux() {
    [[ -n "${PREFIX:-}" && "$PREFIX" == *com.termux* ]] \
        || [[ -d /data/data/com.termux/files/usr ]]
}

# Returns 0 iff `clewdr service uninstall` has a backend it can talk
# to on this host AND the caller has the privileges it needs. Mirror
# of the gate in install.sh — Termux:Boot needs no root, systemd
# does. A non-root user on a systemd host who installed binary-only
# (e.g. CLEWDR_NO_SERVICE=1 + CLEWDR_INSTALL_DIR=$HOME/clewdr) would
# otherwise hit `service uninstall` -> require_root() -> set -e
# aborts before the binary cleanup. Kept verbatim instead of sourced
# — the two scripts are delivered independently and small duplication
# is cheaper than trying to share a library file across the curl|bash
# entry points.
service_supported() {
    if is_termux; then
        return 0
    fi
    if [[ "$(uname -s)" == "Linux" ]] && [[ -d /run/systemd/system ]] && (( EUID == 0 )); then
        return 0
    fi
    return 1
}

default_bin_link_path() {
    if is_termux; then
        echo "${PREFIX:-/data/data/com.termux/files/usr}/bin/clewdr"
    else
        echo "/usr/local/bin/clewdr"
    fi
}

confirm_local_purge() {
    warn "--purge 将删除本机 clewdr 数据："
    warn "  - ${install_dir}/clewdr.toml"
    warn "  - ${install_dir}/clewdr.db"
    warn "  - ${install_dir}/log/"
    if is_termux; then
        warn "  - \$HOME/.local/clewdr/log/"
    fi

    if [[ ! -r /dev/tty || ! -w /dev/tty ]]; then
        err "无法进行 --purge 二次确认；请在交互终端中重试，或去掉 --purge"
    fi
    printf "输入 'yes' 确认删除： " > /dev/tty
    local answer=""
    read -r answer < /dev/tty
    [[ "$answer" == "yes" ]] || err "已取消删除（输入不是 'yes'）"
}

purge_local_data() {
    [[ "$PURGE" == "1" ]] || return 0
    [[ "$service_called" != "1" ]] || return 0

    confirm_local_purge
    rm -f "${install_dir}/clewdr.toml"
    rm -f "${install_dir}/clewdr.db"
    rm -rf "${install_dir}/log"
    if is_termux; then
        rm -rf "${HOME}/.local/clewdr/log"
        rmdir "${HOME}/.local/clewdr" 2>/dev/null || true
    fi
}

# Find the install dir. Honors the env override; otherwise probes the
# two install.sh defaults in priority order. We deliberately don't
# fall back to `which clewdr` — the binary on $PATH might be a
# different (e.g. cargo install'd) copy that this script shouldn't
# touch.
resolve_install_dir() {
    if [[ -n "$INSTALL_DIR_OVERRIDE" ]]; then
        echo "$INSTALL_DIR_OVERRIDE"
        return
    fi
    if [[ -x /opt/clewdr/clewdr ]]; then
        echo /opt/clewdr
        return
    fi
    if [[ -x "${HOME}/clewdr/clewdr" ]]; then
        echo "${HOME}/clewdr"
        return
    fi
    err "couldn't find a clewdr install at /opt/clewdr or \$HOME/clewdr — set CLEWDR_INSTALL_DIR if you installed elsewhere"
}

install_dir=$(resolve_install_dir)
bin="${install_dir}/clewdr"
[[ -x "$bin" ]] || err "no executable at $bin"

if [[ -z "$BIN_LINK_PATH" ]]; then
    BIN_LINK_PATH=$(default_bin_link_path)
fi

info "found install at ${BOLD}${install_dir}${RESET}"

# 1) Service uninstall — let the verb tear down the unit / boot script
# and (with --purge) the data dir. Forwarding --purge here means the
# data confirmation prompt runs inside the verb, where it lives next
# to the path constants the verb already knows.
#
# Skip the call when:
#   - --no-service / CLEWDR_NO_SERVICE=1 was passed (operator did a
#     binary-only install and asks for the matching uninstall);
#   - or no service backend is available with current privileges
#     (macOS, no-systemd Linux, non-root user on a systemd host).
# Otherwise the verb errors out before binary removal and `set -e`
# aborts cleanup entirely. If --purge was requested while the verb was
# skipped, purge_local_data below deletes the install.sh-managed local
# data paths after its own TTY confirmation.
service_called=0
if [[ "$NO_SERVICE" == "1" ]]; then
    info "skipping \`$bin service uninstall\` (CLEWDR_NO_SERVICE / --no-service)"
elif service_supported; then
    if [[ "$PURGE" == "1" ]]; then
        info "calling: $bin service uninstall --purge"
        "$bin" service uninstall --purge
    else
        info "calling: $bin service uninstall"
        "$bin" service uninstall
    fi
    service_called=1
else
    info "no usable service backend for this user (no Termux, no systemd, or systemd needs root); skipping \`$bin service uninstall\`."
    info "if you installed with sudo + systemd, re-run this script with sudo to tear the unit down too."
fi

purge_local_data

# 2) Remove the binary itself + the libc++_shared.so we may have
# placed alongside it on Android. Same root-required rule as install.sh
# for /opt or /usr paths.
if [[ "$install_dir" == /opt/* || "$install_dir" == /usr/* ]] && [[ $EUID -ne 0 ]]; then
    err "removing ${install_dir} requires root — re-run with sudo"
fi

rm -f "$bin"
rm -f "${install_dir}/clewdr.exe"
rm -f "${install_dir}/libc++_shared.so"

if [[ -L "$BIN_LINK_PATH" ]]; then
    link_target=$(readlink "$BIN_LINK_PATH" 2>/dev/null || true)
    if [[ "$link_target" == "$bin" || "$link_target" == "${install_dir}/clewdr" ]]; then
        rm -f "$BIN_LINK_PATH"
        info "removed PATH link ${BIN_LINK_PATH}"
    fi
fi

# Try to remove the install dir, but only if it's empty. If --purge
# wasn't passed, clewdr.db / clewdr.toml / log/ are still in there —
# rmdir will fail and we leave the dir alone, which is what we want
# (those files belong to the user, not this script).
rmdir "$install_dir" 2>/dev/null || true

info "${GREEN}${BOLD}uninstall complete${RESET}"
if [[ "$PURGE" != "1" ]]; then
    info "data preserved (clewdr.db / clewdr.toml / log/ are untouched)."
    info "re-run with --purge to delete them."
fi
