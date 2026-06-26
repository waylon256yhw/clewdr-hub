#!/usr/bin/env bash
# release-docker.sh — 在本地构建并发布多架构 (amd64 + arm64) Docker 镜像到 GHCR。
#
# 本地替代 .github/workflows/docker-build.yml，经 buildx 一次性产出多架构 manifest 并推送。
# 两种构建引擎：
#
#   zig（默认，快）：用 cargo-zigbuild 在宿主原生交叉编译出各架构二进制（zig 自带
#       现代 clang/llvm，能编 BoringSSL/aws-lc），再用 scripts/Dockerfile.dist 仅
#       COPY 二进制进 debian:trixie-slim。arm64 不再在 QEMU 下模拟编译。
#       本机 8GB/3核 实测：arm64 约 3 分钟、峰值内存 ~1.3GB（QEMU 方案要 ~33 分钟）。
#
#   qemu（--qemu，稳）：直接用项目根 Dockerfile 经 buildx --platform 构建，arm64 在
#       QEMU 下编译（慢、但与 CI 镜像字节级一致）。作为 zig 引擎出问题时的回退。
#
# 实测对比（本机 3核/8GB 串行 vs docker-build.yml 在原生 runner 并行 + buildcache）：
#                          amd64        arm64        合计 wall-clock
#   本地 zig（冷）          ~162s        ~167s        ~6m（串行）
#   本地 zig（热,仅应用层）  ~40s         ~39s         ~2m
#   本地 qemu（冷）         原生~3-10m    ~33m         半小时+（仅回退用）
#   CI 热（v1.3.5）        138s         155s         3m10s（两架构并行）
#   CI 冷（v1.2.22）       578s         542s         ~10m
#   结论：zig 引擎下本机出多架构镜像与 CI 同量级、冷构建甚至更快（无仿真+无 CI 开销），
#         代价是占用本机；CI 胜在异步免费、并行不随架构数变差。日常发版仍走标准 CI release。
#
# 用法:
#   ./scripts/release-docker.sh [选项]
#     --qemu            用 QEMU+根 Dockerfile 构建（默认 zig 交叉编译）
#     --version X.Y.Z   覆盖版本标签（默认取 Cargo.toml [package] version）
#     --image NAME      覆盖镜像名（默认 ghcr.io/<owner>/<repo>，全小写）
#     --platforms LIST  覆盖平台（默认 linux/amd64,linux/arm64）
#     --no-latest       不附带 latest 标签
#     --skip-frontend   复用现有 static/（仅 zig 引擎；跳过 npm 构建）
#     --skip-build      复用 dist/docker/ 下已有二进制（仅 zig 引擎；跳过编译）
#     --load-amd64      只构建 amd64 并 docker load 到本地（冒烟用，不推送）
#     --no-push         构建多架构但不推送
#     -h, --help        显示本帮助
#
# 前置依赖:
#   docker(+buildx)。推送需已 gh 登录且 token 含 write:packages 作用域。
#   zig 引擎还需 cargo + rustup；缺 cargo-zigbuild / zig 时脚本会自动安装到缓存。

set -euo pipefail
cd "$(dirname "$0")/.."

note() { echo "==> $*"; }
warn() { echo "==> [警告] $*" >&2; }
err()  { echo "==> [错误] $*" >&2; }
die()  { err "$*"; exit 1; }

# --- 默认参数 ---
ENGINE="zig"
VERSION=""
IMAGE=""
PLATFORMS="linux/amd64,linux/arm64"
WITH_LATEST=true
SKIP_FRONTEND=false
SKIP_BUILD=false
LOAD_AMD64=false
DO_PUSH=true

BUILDER_NAME="clewdr-local"
DIST_DIR="dist/docker"
DOCKERFILE_DIST="scripts/Dockerfile.dist"
DOCKERFILE_FULL="Dockerfile"
FEATURE_ARGS=(--no-default-features --features embed-resource,xdg,tui,encrypt)
PROFILE="release-ci"

# zig 工具链（缺失时自动安装到这里）
ZIG_VERSION="${ZIG_VERSION:-0.16.0}"
ZIG_CACHE=".cache/zig"

declare -A TRIPLE=(
  [amd64]="x86_64-unknown-linux-gnu"
  [arm64]="aarch64-unknown-linux-gnu"
)

while [ $# -gt 0 ]; do
  case "$1" in
    --qemu)       ENGINE="qemu"; shift ;;
    --version)    VERSION="${2:?--version 需要参数}"; shift 2 ;;
    --image)      IMAGE="${2:?--image 需要参数}"; shift 2 ;;
    --platforms)  PLATFORMS="${2:?--platforms 需要参数}"; shift 2 ;;
    --no-latest)  WITH_LATEST=false; shift ;;
    --skip-frontend) SKIP_FRONTEND=true; shift ;;
    --skip-build) SKIP_BUILD=true; shift ;;
    --load-amd64) LOAD_AMD64=true; shift ;;
    --no-push)    DO_PUSH=false; shift ;;
    -h|--help)    sed -n '2,38p' "$0"; exit 0 ;;
    *)            die "未知参数: $1（--help 查看用法）" ;;
  esac
done

# --load-amd64 是单架构本地冒烟
if $LOAD_AMD64; then
  PLATFORMS="linux/amd64"; DO_PUSH=false; WITH_LATEST=false
fi

# 解析平台 → 架构数组 + 是否需要 QEMU（含 arm64）
IFS=',' read -r -a PLATFORM_ARR <<< "$PLATFORMS"
ARCHES=(); NEED_QEMU=false
for p in "${PLATFORM_ARR[@]}"; do
  a="${p#linux/}"
  [ -n "${TRIPLE[$a]:-}" ] || die "不支持的平台: $p（仅 linux/amd64、linux/arm64）"
  ARCHES+=("$a")
  [ "$a" = "arm64" ] && NEED_QEMU=true
done

# --- 公共预检 ---
command -v docker >/dev/null 2>&1 || die "缺少 docker"
docker buildx version >/dev/null 2>&1 || die "缺少 docker buildx 插件"

# --- 版本与镜像名 ---
if [ -z "$VERSION" ]; then
  VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
  [ -n "$VERSION" ] || die "无法从 Cargo.toml 解析版本，请用 --version 指定"
fi
TAG="v${VERSION#v}"

if [ -z "$IMAGE" ]; then
  origin="$(git remote get-url origin 2>/dev/null || true)"
  slug="$(printf '%s' "$origin" | sed -E 's#.*github.com[:/]+([^/]+/[^/]+?)(\.git)?/?$#\1#')"
  [ -n "$slug" ] && [ "$slug" != "$origin" ] || die "无法从 origin 推断镜像名，请用 --image 指定"
  IMAGE="ghcr.io/$(printf '%s' "$slug" | tr '[:upper:]' '[:lower:]')"
fi

TAG_ARGS=(-t "${IMAGE}:${TAG}")
$WITH_LATEST && TAG_ARGS+=(-t "${IMAGE}:latest")

if $DO_PUSH; then OUT_LABEL="是 (推送 GHCR)";
elif $LOAD_AMD64; then OUT_LABEL="否 (docker load 到本地)";
else OUT_LABEL="否 (留在 buildx 缓存)"; fi
LATEST_LABEL=""; $WITH_LATEST && LATEST_LABEL=" + latest"

note "引擎:     $ENGINE$([ "$ENGINE" = zig ] && echo ' (交叉编译)' || echo ' (QEMU)')"
note "镜像:     $IMAGE"
note "标签:     ${TAG}${LATEST_LABEL}"
note "平台:     $PLATFORMS"
note "输出:     $OUT_LABEL"

# ============================================================================
# zig 引擎：交叉编译 → 瘦镜像
# ============================================================================
ensure_zig_toolchain() {
  command -v cargo >/dev/null 2>&1 || die "缺少 cargo（安装: https://rustup.rs）"
  command -v rustup >/dev/null 2>&1 || die "缺少 rustup（zig 引擎需要它装交叉 std）"

  # rust 交叉 std
  local installed; installed="$(rustup target list --installed 2>/dev/null)"
  for arch in "${ARCHES[@]}"; do
    if ! grep -qx "${TRIPLE[$arch]}" <<< "$installed"; then
      note "添加 rust target: ${TRIPLE[$arch]}"
      rustup target add "${TRIPLE[$arch]}"
    fi
  done

  # cargo-zigbuild
  command -v cargo-zigbuild >/dev/null 2>&1 || { note "安装 cargo-zigbuild ..."; cargo install cargo-zigbuild --locked; }

  # zig：优先 PATH，其次缓存，否则下载到缓存
  if command -v zig >/dev/null 2>&1; then
    ZIG_BIN="$(command -v zig)"
  else
    local host; host="$(uname -m)"   # x86_64 / aarch64
    local dir="$ZIG_CACHE/zig-${host}-linux-${ZIG_VERSION}"
    ZIG_BIN="$dir/zig"
    if [ ! -x "$ZIG_BIN" ]; then
      local url="https://ziglang.org/download/${ZIG_VERSION}/zig-${host}-linux-${ZIG_VERSION}.tar.xz"
      note "下载 zig ${ZIG_VERSION} → $ZIG_CACHE ..."
      mkdir -p "$ZIG_CACHE"
      curl -fL --retry 3 -o "$ZIG_CACHE/zig.tar.xz" "$url" || die "zig 下载失败: $url"
      tar -C "$ZIG_CACHE" -xf "$ZIG_CACHE/zig.tar.xz" || die "zig 解包失败"
      rm -f "$ZIG_CACHE/zig.tar.xz"
      [ -x "$ZIG_BIN" ] || die "zig 解包后未找到可执行文件: $ZIG_BIN"
    fi
  fi
  export PATH="$(dirname "$ZIG_BIN"):$PATH"
  note "zig: $("$ZIG_BIN" version)  cargo-zigbuild: 就绪"
}

build_zig() {
  if ! $SKIP_BUILD; then
    ensure_zig_toolchain

    if ! $SKIP_FRONTEND; then
      note "构建前端 static/ ..."
      if [ -f frontend/package-lock.json ]; then npm --prefix frontend ci; else npm --prefix frontend install; fi
      npm --prefix frontend run build
    fi
    [ -f static/index.html ] || die "static/index.html 缺失（去掉 --skip-frontend）"

    for arch in "${ARCHES[@]}"; do
      local triple="${TRIPLE[$arch]}"
      note "交叉编译 $arch ($triple) ..."
      cargo zigbuild --profile "$PROFILE" --target "$triple" "${FEATURE_ARGS[@]}" --bin clewdr
      mkdir -p "$DIST_DIR/$arch"
      cp "target/$triple/$PROFILE/clewdr" "$DIST_DIR/$arch/clewdr"
    done
  fi
  for arch in "${ARCHES[@]}"; do
    [ -f "$DIST_DIR/$arch/clewdr" ] || die "缺少二进制 $DIST_DIR/$arch/clewdr（去掉 --skip-build）"
  done

  # arm64 瘦镜像运行阶段的 apt 需要 QEMU
  $NEED_QEMU && ensure_qemu
  ensure_builder
  $DO_PUSH && ghcr_login

  local out=(); $DO_PUSH && out=(--push); $LOAD_AMD64 && out=(--load)
  note "buildx 打包瘦镜像 ..."
  docker buildx --builder "$BUILDER_NAME" build \
    --platform "$PLATFORMS" -f "$DOCKERFILE_DIST" \
    "${TAG_ARGS[@]}" "${out[@]}" "$DIST_DIR"
}

# ============================================================================
# qemu 引擎：根 Dockerfile 直接多架构构建
# ============================================================================
build_qemu() {
  [ -f "$DOCKERFILE_FULL" ] || die "找不到 $DOCKERFILE_FULL"
  $NEED_QEMU && warn "含 arm64：QEMU 下编译，本机冷构建约半小时；建议保留几 GB swap 兜底"
  $NEED_QEMU && ensure_qemu
  ensure_builder
  $DO_PUSH && ghcr_login

  local out=(); $DO_PUSH && out=(--push); $LOAD_AMD64 && out=(--load)
  note "buildx 构建镜像（首次冷编译较久）..."
  docker buildx --builder "$BUILDER_NAME" build \
    --platform "$PLATFORMS" -f "$DOCKERFILE_FULL" \
    "${TAG_ARGS[@]}" "${out[@]}" .
}

# ============================================================================
# 公共子过程
# ============================================================================
ensure_qemu() {
  if ! ls /proc/sys/fs/binfmt_misc/ 2>/dev/null | grep -qi qemu-aarch64; then
    note "注册 QEMU binfmt（arm64 所需）..."
    docker run --privileged --rm tonistiigi/binfmt --install arm64 >/dev/null
  fi
}

ensure_builder() {
  if ! docker buildx inspect "$BUILDER_NAME" >/dev/null 2>&1; then
    note "创建 buildx builder: $BUILDER_NAME (docker-container)"
    docker buildx create --name "$BUILDER_NAME" --driver docker-container >/dev/null
  fi
}

ghcr_login() {
  command -v gh >/dev/null 2>&1 || die "需要 gh 获取 GHCR 凭据（或自行 docker login ghcr.io）"
  local owner; owner="$(printf '%s' "$IMAGE" | sed -E 's#ghcr.io/([^/]+)/.*#\1#')"
  note "登录 GHCR (用户: $owner) ..."
  gh auth token | docker login ghcr.io -u "$owner" --password-stdin \
    || die "GHCR 登录失败：确认 gh token 含 write:packages（gh auth refresh -s write:packages）"
}

# --- 执行 ---
case "$ENGINE" in
  zig)  build_zig ;;
  qemu) build_qemu ;;
esac

note "完成 ✓"
if $DO_PUSH; then
  note "已推送多架构 manifest: ${IMAGE}:${TAG}${LATEST_LABEL}"
  note "校验: docker buildx imagetools inspect ${IMAGE}:${TAG}"
elif $LOAD_AMD64; then
  note "已 load 到本地: ${IMAGE}:${TAG}"
  note "试跑: docker run --rm -p 8484:8484 ${IMAGE}:${TAG}"
else
  note "已构建但未推送（--no-push）。去掉 --no-push 即可发布到 GHCR。"
fi
