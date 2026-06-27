---
title: 构建与发布
description: 本地构建的 feature 组合、release.sh 发版流程、失败恢复，以及 pre-commit hook。
sidebar:
  order: 10
---

## 本地构建

```bash
cargo build --release                                                            # 开发默认 feature
cargo build --release --no-default-features --features embed-resource,xdg        # Docker 风格
cargo build --release --no-default-features --features embed-resource,portable   # Release 风格
```

feature 含义见[前端 · 与后端集成](../frontend/#与后端集成)。

## 发布流程

版本号遵循 semver（`MAJOR.MINOR.PATCH`）。Bug fix → patch，新功能 → minor，破坏性变更 → major。

### 前置：安装发布工具

`release.sh` 依赖 `cargo set-version`，由 `cargo-edit` 提供。首次在新节点发版前安装一次即可：

```bash
./scripts/setup-dev.sh --release-tools
# 或手动安装:
cargo install cargo-edit --locked
```

### 发版步骤

1. **确认工作区干净**：`git status` 应该只剩需要发版的已 commit 变更，`git log origin/master..HEAD` 看一眼待发布的 commit。

2. **运行 `./release.sh X.Y.Z`**（注意不带 `v` 前缀）：

   ```bash
   ./release.sh 1.0.16
   ```

   脚本依次执行：

   - `cargo update`（刷新 `Cargo.lock` 里的依赖 patch 版本）
   - 检查 `cargo set-version` 是否可用（缺失时提示安装 `cargo-edit`）
   - `cargo set-version X.Y.Z`（同步改 `Cargo.toml` 和 `Cargo.lock` 里的包版本）
   - `cargo test`
   - `cd frontend && npm ci && npm run build`（产物写到 `static/`）
   - `cargo check`
   - `git add Cargo.toml Cargo.lock && git commit -m "Update to vX.Y.Z"`
   - `git push`（推 master）
   - `git tag -a vX.Y.Z -m "Release vX.Y.Z"`
   - `git push origin vX.Y.Z`（推 tag）

   任何一步失败会直接 `set -e` 退出；失败后按下面的「失败恢复」处理。Changelog 不需要手动维护——CI 会通过 git-cliff 从 conventional commits 自动生成当前版本的变更日志作为 GitHub Release body。

3. **验证 CI**：

   ```bash
   gh run list --limit 5                 # 看 build / Docker workflow 状态
   gh run watch <run-id>                 # 跟踪某个 run 的实时日志
   gh release view vX.Y.Z                # release 创建成功后可见
   ```

   tag push 会触发两条 workflow：

   - **build.yml**：跨平台编译二进制 → `softprops/action-gh-release@v2` 自动创建 GitHub Release
   - **docker-build.yml**：多架构 Docker 镜像 → `ghcr.io/waylon256yhw/clewdr-hub:vX.Y.Z`。需要在本地一次性出多架构镜像时，见下方[本地多架构 Docker 构建](#本地多架构-docker-构建)。

### 失败恢复

| 情况 | 处理 |
|---|---|
| `cargo test` / 前端构建 / `cargo check` 失败 | 修代码 → `git add` → `git commit --amend` 或新 commit → 重新跑 `./release.sh`。此时 Cargo.toml 里已经是目标版本，`cargo set-version` 幂等，不会冲突。 |
| 脚本跑到一半（已 commit 未 push）失败 | 检查 `git log`，如果「Update to vX.Y.Z」这个 commit 已经存在且内容正确，直接手动执行剩余的 `git push`、`git tag`、`git push origin vX.Y.Z`。 |
| tag 已推送但 CI 构建失败 | 先在 GitHub 上删掉对应 release 和 tag：`gh release delete vX.Y.Z --yes --cleanup-tag`；本地 `git tag -d vX.Y.Z`；修 bug → 补 commit → 重新 `./release.sh X.Y.Z`（版本号不变，因为原 tag 没有任何东西依赖它）。 |
| Release body 内容有误 | Release 创建后，用 `gh release edit vX.Y.Z --notes "corrected content"` 更新 body；不需要重打 tag。 |

### 设计说明

- **脚本不自建 GitHub Release**：创建动作完全委托给 `build.yml` 里的 `softprops/action-gh-release@v2`，避免本地 `gh release create` 和 CI 并发创建导致冲突。
- **Release body 由 git-cliff 自动生成**：CI 中通过 `orhun/git-cliff-action` 从上一个 tag 到当前 tag 之间的 conventional commits 生成 changelog，只包含当前版本的变更。配置见 `cliff.toml`。
- **版本号同时写在 `Cargo.toml` 和 `Cargo.lock`**：`cargo set-version` 两处都改；手动 bump 时别漏了 `Cargo.lock` 第二处 `[[package]] name = "clewdr-hub"` 的 `version` 字段。

## 本地多架构 Docker 构建

`scripts/release-docker.sh` 是 `docker-build.yml` 的**本地替代**：经 buildx 一次性产出多架构（amd64 + arm64）manifest 并推送到 GHCR。

> **定位**：日常发版仍走标准 CI（`release.sh` 打 tag → `docker-build.yml` 异步免费、并行构建）。本工具是需要在本地立刻出多架构镜像时的补充——冷构建甚至比 CI 更快（无仿真、无 CI 排队开销），代价是占用本机。

### 两种引擎

| 引擎 | 触发 | 原理 | arm64 冷构建（本机 3核/8GB） |
|---|---|---|---|
| **zig**（默认，快） | 缺省 | `cargo-zigbuild` 在宿主交叉编译各架构二进制（zig 自带现代 clang/llvm，能编 BoringSSL/aws-lc），再用 `scripts/Dockerfile.dist` 仅 `COPY` 二进制进 `debian:trixie-slim`。arm64 不经 QEMU 编译。 | ~3 分钟，峰值内存 ~1.3GB |
| **qemu**（`--qemu`，稳） | 显式 | 直接用项目根 `Dockerfile` 经 buildx `--platform` 构建，arm64 在 QEMU 下编译——慢，但与 CI 镜像字节级一致。zig 引擎出问题时的回退。 | ~33 分钟 |

### 前置依赖

- `docker` + `buildx` 插件。
- 推送 GHCR：已 `gh` 登录，且 token 含 `write:packages` 作用域（缺则 `gh auth refresh -s write:packages`）。脚本用 `gh auth token` 自动 `docker login ghcr.io`。
- zig 引擎额外需 `cargo` + `rustup`（装交叉 std）；缺 `cargo-zigbuild` / `zig` 时脚本自动安装/下载到 `.cache/zig`。

### 常用用法

```bash
./scripts/release-docker.sh                 # zig 交叉编译 amd64+arm64，打 vX.Y.Z + latest，推送 GHCR
./scripts/release-docker.sh --load-amd64    # 只构建 amd64 并 docker load 到本地（冒烟，不推送）
./scripts/release-docker.sh --no-push       # 构建多架构但不推送
./scripts/release-docker.sh --qemu          # 回退到 QEMU + 根 Dockerfile
```

版本默认取 `Cargo.toml` 的 `[package] version`，镜像名默认从 `git remote origin` 推断为 `ghcr.io/<owner>/<repo>`（全小写）。

| 选项 | 作用 |
|---|---|
| `--qemu` | 用 QEMU + 根 Dockerfile 构建（默认 zig 交叉编译） |
| `--version X.Y.Z` | 覆盖版本标签 |
| `--image NAME` | 覆盖镜像名 |
| `--platforms LIST` | 覆盖平台（默认 `linux/amd64,linux/arm64`） |
| `--no-latest` | 不附带 `latest` 标签 |
| `--skip-frontend` | 复用现有 `static/`，跳过 npm 构建（仅 zig 引擎） |
| `--skip-build` | 复用 `dist/docker/` 下已有二进制，跳过编译（仅 zig 引擎） |
| `--load-amd64` | 只构建 amd64 并 `docker load` 到本地（冒烟，不推送） |
| `--no-push` | 构建多架构但不推送 |

推送后校验：

```bash
docker buildx imagetools inspect ghcr.io/waylon256yhw/clewdr-hub:vX.Y.Z
```

## pre-commit hook

`.githooks/pre-commit` 执行 `cargo fmt -- --check`。通过 dev.sh 或手动配置：

```bash
git config core.hooksPath .githooks
```
