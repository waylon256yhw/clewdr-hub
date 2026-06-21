---
title: 开发环境 & dev.sh
description: 搭建 clewdr-hub 本地开发环境，以及 dev.sh 一键开发脚本的用法。
sidebar:
  order: 1
---

面向贡献者和二次开发者。普通使用者请看[入门](../../start/quick-start/)。

## 前置依赖

- Rust（stable, edition 2024）
- Node.js（LTS）+ npm
- SQLite 3（系统自带即可，代码通过 sqlx 内嵌驱动）
- 系统构建库：编译 BoringSSL / bindgen 等原生依赖需要 `cmake`、`clang`、`libclang-dev`、`build-essential`、`perl`、`pkg-config`

一键安装系统依赖（Ubuntu/Debian 自动；其他发行版会打印对应手动命令）：

```bash
./scripts/setup-dev.sh
```

手动安装（apt）：

```bash
sudo apt-get install -y build-essential cmake clang libclang-dev perl pkg-config
```

`dev.sh` 每次启动前会自动跑 `scripts/setup-dev.sh --check` 预检，缺依赖时给出明确提示，而不是 BoringSSL 的底层报错。语言工具链（Rust/Node）需自行安装，脚本只检测不代装。

## dev.sh

一键开发脚本，位于项目根目录：

```bash
./dev.sh                # 仅重启后端（复用已构建的 static/）
./dev.sh rebuild        # 重建前端 + 启动后端
./dev.sh reset          # 删库重建（admin:password）
./dev.sh rebuild reset  # 两者都做
./dev.sh hmr            # 启动后端 + Vite HMR（全栈开发）
./dev.sh stop           # 停止 dev.sh 启动的后端/Vite 进程
./dev.sh no-timeout     # 启动时关闭自动停机 watchdog
./dev.sh timeout=7200   # 启动时将自动停机改为 2 小时
```

脚本行为：

1. 杀掉已有的 clewdr 进程
2. 可选：删除 `clewdr.db` 重新初始化
3. 可选：`cd frontend && npx vite build` 构建前端到 `static/`
4. `cargo build` 前台编译后端（首次冷编译可能数分钟、进度可见、失败即退出），再 `cargo run -- --db clewdr.db` 后台启动（输出写入 `.dev-backend.log`）
5. `hmr` 模式下启动 `npm --prefix frontend run dev`（输出写入 `.dev-frontend.log`）
6. 轮询后端 `/api/version` 和（可选）前端首页等待就绪，超时 60 秒
7. 默认启动自动停机 watchdog，3 小时后自动执行 `./dev.sh stop`

首次运行会自动触发前端构建（检测 `static/index.html` 不存在时）。可通过 `DEV_AUTO_STOP_SECONDS` 环境变量改默认超时；设为 `0` 可全局关闭 watchdog。`timeout=SECONDS` 必须是正整数，传错会直接报错退出。

脚本还会自动配置 `git config core.hooksPath .githooks`，启用 pre-commit 的 `cargo fmt --check`。

## 前端独立开发

```bash
cd frontend
npm install
npm run dev         # Vite dev server，自动代理 /api → localhost:8484
```

需要后端同时运行。Vite 代理默认转发到 `http://localhost:8484`。可通过环境变量 `VITE_DEV_BACKEND_URL` 覆盖。

## 调试配置

常用开发配置写在根目录 `clewdr.toml`，也可通过 `CLEWDR_` 前缀环境变量覆盖。

- `debug_cookie = false`（默认）：控制手动 `probe_cookie` 是否额外把原始上游 JSON dump 到 `log/probe-dumps/`。关闭时，`request_logs` 只保存 `bootstrap_summary + usage`；开启时，日志里会额外出现 `debug_dump_file` / `debug_component_bytes`，便于排查超大 bootstrap 响应。推荐只在临时排障时通过 `CLEWDR_DEBUG_COOKIE=true` 或直接修改 `clewdr.toml` 开启，不要做成后台常驻开关。
- `no_fs = false`（默认）：遗留的「无文件系统」运行模式。开启后会切到内存 SQLite（`:memory:`），并关闭配置落盘、文件日志、JSON dump 等所有文件写入行为。该模式不适合当前后台管理/调试工作流，日常开发与排障不要开启。
