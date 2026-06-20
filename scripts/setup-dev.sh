#!/usr/bin/env bash
# setup-dev.sh — 安装/校验 clewdr-hub 本地开发所需的系统依赖。
#
# 系统依赖来源 = Dockerfile 的 backend-builder 阶段（单一事实源）：
#   编译 BoringSSL(boring-sys2)/aws-lc-sys 需要 cmake + clang，
#   bindgen 需要 libclang(+ 内建头)，多个 -sys crate 需要 C 编译器/perl/pkg-config。
#
# 用法:
#   ./scripts/setup-dev.sh            检测并安装缺失的系统依赖
#                                     （apt 自动安装；其他包管理器仅打印手动命令）
#   ./scripts/setup-dev.sh --check    仅检测、不安装；缺失则非零退出（dev.sh 预检复用）

set -euo pipefail
cd "$(dirname "$0")/.."

CHECK_ONLY=false
for arg in "$@"; do
  case "$arg" in
    --check) CHECK_ONLY=true ;;
    -h|--help)
      echo "用法: ./scripts/setup-dev.sh [--check]"
      echo "  (无参数)  检测并安装缺失的系统依赖（apt 自动）"
      echo "  --check   仅检测，不安装；缺失则非零退出"
      exit 0
      ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done

note() { echo "==> $*"; }
warn() { echo "==> [警告] $*" >&2; }
err()  { echo "==> [错误] $*" >&2; }

# apt 包名（与 Dockerfile backend-builder 阶段保持一致）—— 安装模式按此集合补齐。
APT_PKGS=(build-essential cmake clang libclang-dev perl pkg-config)
# 功能性探针：检测构建真正会调用的工具，而非具体包名。
# 一台机器即便没装 clang / libclang-dev 元包，只要 libclang.so + 头文件在也能构建。
REQUIRED_CMDS=(cmake cc c++ make perl pkg-config)

# bindgen/clang-sys 通过 dlopen 加载 libclang（不依赖 clang 可执行文件）。
# 按 clang-sys 的查找顺序探测，避免漏判 Nix/Homebrew/自定义 LLVM 等不在
# ldconfig 缓存里的安装：LIBCLANG_PATH → llvm-config → ldconfig → clang 兜底。
has_libclang() {
  # 1) 显式 LIBCLANG_PATH：可指向 libclang 文件，或含 libclang.* 的目录
  if [ -n "${LIBCLANG_PATH:-}" ]; then
    [ -f "$LIBCLANG_PATH" ] && return 0
    if [ -d "$LIBCLANG_PATH" ] && \
       ( shopt -s nullglob; set -- "$LIBCLANG_PATH"/libclang.*; [ "$#" -gt 0 ] ); then
      return 0
    fi
  fi
  # 2) llvm-config 在 PATH（Nix/Homebrew/自定义 LLVM 安装）
  command -v llvm-config >/dev/null 2>&1 && return 0
  # 3) Linux 系统安装：ldconfig 缓存
  if command -v ldconfig >/dev/null 2>&1 && ldconfig -p 2>/dev/null | grep -qi libclang; then
    return 0
  fi
  # 4) 兜底：clang 可执行（覆盖 macOS / 部分发行版）
  command -v clang >/dev/null 2>&1
}

# --- 检测缺失的系统依赖 ---
missing=()
for c in "${REQUIRED_CMDS[@]}"; do
  command -v "$c" >/dev/null 2>&1 || missing+=("$c")
done
has_libclang || missing+=("libclang")

# --- 检测语言工具链（脚本不代为安装，仅提示）---
toolchain=()
command -v cargo >/dev/null 2>&1 || toolchain+=("Rust 工具链 (cargo) —— 安装: https://rustup.rs")
command -v node  >/dev/null 2>&1 || toolchain+=("Node.js LTS (node) —— 安装: https://nodejs.org 或 fnm")
command -v npm   >/dev/null 2>&1 || toolchain+=("npm（随 Node.js 一同安装）")

print_toolchain() {
  [ ${#toolchain[@]} -eq 0 ] && return 0
  warn "缺少语言工具链（需手动安装）:"
  local t
  for t in "${toolchain[@]}"; do warn "  - $t"; done
}

# --- --check：只报告，不安装 ---
if $CHECK_ONLY; then
  rc=0
  if [ ${#missing[@]} -gt 0 ]; then
    err "缺少系统构建依赖: ${missing[*]}"
    err "运行 ./scripts/setup-dev.sh 自动安装"
    rc=1
  fi
  if [ ${#toolchain[@]} -gt 0 ]; then
    print_toolchain
    rc=1
  fi
  [ $rc -eq 0 ] && note "开发依赖齐全 ✓"
  exit $rc
fi

# --- 安装模式 ---
print_toolchain   # 工具链缺失只警告，不阻断系统依赖安装

install_apt() {
  local missing_pkgs=() p
  for p in "${APT_PKGS[@]}"; do
    dpkg -s "$p" >/dev/null 2>&1 || missing_pkgs+=("$p")
  done
  if [ ${#missing_pkgs[@]} -eq 0 ]; then
    note "系统依赖已齐全: ${APT_PKGS[*]}"
    return 0
  fi
  note "将安装缺失的系统依赖: ${missing_pkgs[*]}"
  local sudo=""
  [ "$(id -u)" -ne 0 ] && sudo="sudo"
  $sudo apt-get update
  $sudo apt-get install -y "${missing_pkgs[@]}"
}

print_manual_hint() {
  err "未检测到 apt，未自动安装。请用你的包管理器安装等价依赖:"
  err "  需要: cmake / clang+libclang(含头文件) / C++ 编译器(make) / perl / pkg-config"
  if command -v dnf >/dev/null 2>&1; then
    err "  Fedora:   sudo dnf install -y gcc gcc-c++ make cmake clang clang-devel perl pkgconf-pkg-config"
  elif command -v pacman >/dev/null 2>&1; then
    err "  Arch:     sudo pacman -S --needed base-devel cmake clang perl pkgconf"
  elif command -v zypper >/dev/null 2>&1; then
    err "  openSUSE: sudo zypper install -y gcc gcc-c++ make cmake clang llvm-devel perl pkg-config"
  elif command -v brew >/dev/null 2>&1; then
    err "  macOS:    xcode-select --install && brew install cmake llvm perl pkg-config"
  fi
}

if command -v apt-get >/dev/null 2>&1; then
  install_apt
elif [ ${#missing[@]} -gt 0 ]; then
  print_manual_hint
  exit 1
else
  note "系统依赖已齐全（非 apt 系统，跳过自动安装）"
fi

# --- 安装后复检 ---
recheck=()
for c in "${REQUIRED_CMDS[@]}"; do
  command -v "$c" >/dev/null 2>&1 || recheck+=("$c")
done
has_libclang || recheck+=("libclang")
if [ ${#recheck[@]} -gt 0 ]; then
  err "安装后仍缺少: ${recheck[*]}（请检查上面的安装输出）"
  exit 1
fi

note "系统依赖就绪 ✓"
if [ ${#toolchain[@]} -eq 0 ]; then
  note "全部开发依赖就绪，可运行 ./dev.sh"
else
  warn "系统依赖就绪；请按上面提示补齐语言工具链后再运行 ./dev.sh"
fi
