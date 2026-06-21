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
   - **docker-build.yml**：多架构 Docker 镜像 → `ghcr.io/waylon256yhw/clewdr-hub:vX.Y.Z`

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

## pre-commit hook

`.githooks/pre-commit` 执行 `cargo fmt -- --check`。通过 dev.sh 或手动配置：

```bash
git config core.hooksPath .githooks
```
