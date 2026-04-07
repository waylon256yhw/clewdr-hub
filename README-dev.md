# clewdr-hub 开发文档

面向贡献者和二次开发者的技术参考。用户请看 [README.md](README.md)。

---

## 目录

- [开发环境](#开发环境)
- [项目结构](#项目结构)
- [架构概览](#架构概览)
- [请求生命周期](#请求生命周期)
- [账号调度器](#账号调度器)
- [认证体系](#认证体系)
- [数据库](#数据库)
- [前端](#前端)
- [构建与发布](#构建与发布)
- [已知问题与设计决策](#已知问题与设计决策)

---

## 开发环境

### 前置依赖

- Rust (stable, edition 2024)
- Node.js (LTS) + npm
- SQLite 3（系统自带即可，代码通过 sqlx 内嵌驱动）

### dev.sh

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
4. `cargo run -- --db clewdr.db` 后台启动后端（输出写入 `.dev-backend.log`）
5. `hmr` 模式下启动 `npm --prefix frontend run dev`（输出写入 `.dev-frontend.log`）
6. 轮询后端 `/api/version` 和（可选）前端首页等待就绪，超时 60 秒
7. 默认启动自动停机 watchdog，3 小时后自动执行 `./dev.sh stop`

首次运行会自动触发前端构建（检测 `static/index.html` 不存在时）。
可通过 `DEV_AUTO_STOP_SECONDS` 环境变量改默认超时；设为 `0` 可全局关闭 watchdog。
`timeout=SECONDS` 必须是正整数，传错会直接报错退出。

脚本还会自动配置 `git config core.hooksPath .githooks`，启用 pre-commit 的 `cargo fmt --check`。

### 前端独立开发

```bash
cd frontend
npm install
npm run dev         # Vite dev server，自动代理 /api → localhost:8484
```

需要后端同时运行。Vite 代理默认转发到 `http://localhost:8484`。可通过环境变量 `VITE_DEV_BACKEND_URL` 覆盖。

---

## 项目结构

```
src/
├── main.rs                    # 入口：CLI 参数、DB 初始化、启动 HTTP server
├── lib.rs                     # 模块注册
├── config/                    # 配置、常量、CookieStatus 结构体
│   ├── constants.rs           # DB_PATH / CONFIG_PATH / 全局 LazyLock
│   └── cookie.rs              # CookieStatus：cookie 运行时状态（token、用量、窗口）
├── db/
│   ├── mod.rs                 # init_pool / seed_admin / migrations
│   ├── models.rs              # AuthenticatedUser 等共享类型
│   ├── accounts.rs            # AccountWithRuntime / load_all_accounts / batch_upsert
│   ├── queries.rs             # authenticate_api_key / touch_api_key / touch_user
│   ├── api_key.rs             # Key 生成（blake3 哈希 + lookup 前缀）
│   └── billing.rs             # insert_request_log / upsert_usage_rollup
├── api/
│   ├── claude_code.rs         # POST /v1/messages 主处理器
│   ├── models.rs              # GET /v1/models
│   ├── health.rs              # GET /health
│   ├── auth.rs                # POST /auth/login, /auth/logout
│   └── admin/                 # /api/admin/* 管理 API（accounts, users, keys, policies...）
├── middleware/
│   ├── auth.rs                # RequireFlexibleAuth（API Key）/ RequireAdminAuth（session cookie）
│   └── claude/request.rs      # ClaudeCodePreprocess：请求预处理、billing header 注入
├── providers/claude/mod.rs    # ClaudeProvider：构建 ClaudeCodeState 并调用 try_chat
├── claude_code_state/
│   ├── mod.rs                 # ClaudeCodeState：持有 cookie、client、billing context
│   ├── chat.rs                # try_chat / try_count_tokens / 流式转发 / 用量持久化
│   ├── exchange.rs            # OAuth token 交换（cookie → access_token）
│   ├── organization.rs        # 获取组织 UUID
│   └── probe.rs               # 账号探测（类型、邮箱、用量窗口）
├── services/
│   ├── cookie_actor.rs        # CookieActor（ractor）：调度、回收、inflight 追踪、脏刷盘
│   ├── user_limiter.rs        # UserLimiterMap：per-user 并发 semaphore
│   └── log_rotation.rs        # 日志轮转（默认保留 7 天）
├── router.rs                  # RouterBuilder：路由注册、中间件挂载
├── session.rs                 # HMAC-SHA256 签名 cookie（创建/验证/过期）
├── stealth.rs                 # 伪装 profile（CLI/SDK 版本、请求头构建）
├── billing.rs                 # BillingContext / persist_billing_to_db
├── error.rs                   # ClewdrError（snafu 派生）
└── types/claude.rs            # Claude API 请求/响应类型定义

frontend/src/
├── main.tsx                   # React 入口
├── api.ts                     # API client + TypeScript 接口定义
├── routes/                    # 页面组件（Accounts, Users, Keys, Logs, Settings, Overview）
└── lib/                       # 工具函数
```

---

## 架构概览

```
Claude Code / 任意 Anthropic 客户端
          │
          ▼
   ┌──────────────┐
   │  Axum Router  │  /v1/messages, /v1/models, /api/admin/*
   └──────┬───────┘
          │
   RequireFlexibleAuth          ← API Key 认证（blake3 校验）
   ClaudeCodePreprocess         ← 提取模型、构建 billing context
   UserLimiter                  ← per-user 并发 semaphore + RPM 检查
   QuotaCheck                   ← 周/月预算检查
          │
          ▼
   ┌──────────────┐
   │ ClaudeProvider │  构建 ClaudeCodeState
   └──────┬───────┘
          │
   CookieActor::dispatch()     ← 选账号（bound 过滤 → inflight 检查 → round-robin）
   token 交换/刷新              ← cookie → OAuth access_token
          │
          ▼
   ┌──────────────┐
   │  Claude API   │  api.anthropic.com/v1/messages
   └──────────────┘
```

---

## 请求生命周期

以 `POST /v1/messages` 为例：

1. **认证**（`RequireFlexibleAuth`）：从 `x-api-key` 或 `Authorization: Bearer` 提取 key → 查 `api_keys` 表 → blake3 校验 → 注入 `AuthenticatedUser` 到 request extensions。同时 fire-and-forget 更新 `last_used_at`/`last_used_ip`/`last_seen_at`。

2. **预处理**（`ClaudeCodePreprocess`）：解析请求体 → 提取模型名 → 构建 `ClaudeContext`（request_id, user_id, api_key_id, bound_account_ids, started_at）。

3. **限流**（handler 内）：
   - `UserLimiterMap::try_acquire()`：per-user semaphore，基于策略的 `max_concurrent`
   - RPM 检查：滑动窗口
   - 预算检查：查 `usage_rollups` 表的周/月累计

4. **调度**（`CookieActor::dispatch()`）：
   - `bound_account_ids` 非空时只从绑定账号中选
   - 检查 `inflight < max_slots`
   - moka 缓存命中 → 亲和性返回同一账号
   - 否则 round-robin 取第一个可用账号
   - `inflight += 1`

5. **Token 交换**：检查 cookie 的 access_token 状态（None/Expired/Valid），必要时走 OAuth code → token 流程。

6. **上游请求**：构建伪装请求头（stealth profile）→ `POST api.anthropic.com/v1/messages`

7. **响应处理**：
   - 非流式：读取完整响应 → 提取 usage → 持久化计费 → 返回
   - 流式：返回 SSE stream → 在 `MessageStop` 事件中异步持久化 usage + 释放 slot
   - 流异常终止（客户端断开）：`SlotDropGuard` 的 `Drop` impl 自动释放 slot

8. **重试**：遇到 429/auth 错误时 `return_cookie(Some(reason))` 将账号移入 exhausted 队列 → 换一个账号重试，最多 6 次。

---

## 账号调度器

核心是 `CookieActor`（基于 [ractor](https://github.com/slawlor/ractor) 框架的 actor 模型）。

### 状态

```rust
struct CookieActorState {
    valid: VecDeque<CookieStatus>,        // 可用队列
    exhausted: HashSet<CookieStatus>,     // 冷却中（429）
    invalid: HashSet<UselessCookie>,      // 失效（auth error）
    moka: Cache<u64, CookieStatus>,       // 亲和性缓存（1h TTL）
    inflight: HashMap<i64, (u32, u32)>,   // account_id → (当前并发, max_slots)
    dirty: HashSet<i64>,                  // 待刷盘的 account_id
    db: SqlitePool,
}
```

### 消息类型

| 消息 | 触发方 | 作用 |
|------|--------|------|
| `Request` | 请求处理器 | dispatch 一个 cookie，inflight++ |
| `Return` | 请求结束 | 回收 cookie（更新 or 移入 exhausted/invalid） |
| `ReleaseSlot` | 请求结束 | inflight-- |
| `Submit` | admin API | 添加新 cookie |
| `Delete` | admin API | 删除 cookie |
| `CheckReset` | 定时器 (5min) | 检查 exhausted cookie 是否可以恢复 |
| `FlushDirty` | 定时器 (15s) | 批量写脏数据到 DB |
| `ReloadFromDb` | admin API | 重新加载（保留 inflight 计数） |
| `ProbeAll` | admin API | 重激活所有 disabled 账号并探测 |

### 并发槽（max_slots）

每个账号有独立的 inflight 计数器。`dispatch()` 跳过 `inflight >= max_slots` 的账号。释放路径：

- **非流式请求**：handler 中显式调用 `release_slot()`
- **流式请求正常结束**：`MessageStop` 事件的 spawn 中释放，用 `AtomicBool` 防重复
- **流式请求异常终止**：`SlotDropGuard` 的 `Drop` 通过 `tokio::spawn` 异步释放
- **reload**：保留已有 inflight 计数，只更新 max_slots 值

### 脏刷盘

每 15 秒批量 upsert `account_runtime_state` 表（使用 SQLite transaction），只写 dirty set 中的账号。shutdown 时 `post_stop()` 强制刷全量。

---

## 认证体系

### API Key 认证（用户请求）

Key 格式：`sk-{lookup_prefix}_{random_part}`

- `lookup_prefix`（8 字符）：明文存储，用于快速查找
- 完整 key 的 blake3 哈希存储在 `key_hash` 列
- 认证流程：按 prefix 查行 → 计算提交 key 的 blake3 → 比对

### Admin Session（后台登录）

- admin 密码用 argon2 哈希存储
- 登录成功后签发 HMAC-SHA256 签名 cookie（`clewdr_session`）
- payload: `{user_id}.{session_version}.{expires}`
- TTL 24h，`session_version` 递增即可踢掉所有会话

### 关于 plaintext_key

**已知设计决策**：`api_keys` 表保留了 `plaintext_key` 列，admin 后台可以回看完整 key。

这是有意为之——在高信任小团队场景下，admin 需要能帮成员找回 key，避免"忘了复制就永远丢失"的运维痛点。trade-off 是 DB 泄露等同于所有 key 泄露。如果你的威胁模型需要更强的隔离，可以：

1. 删除 migration 中的 `plaintext_key` 列
2. 移除 `create_api_key()` 中的 `.bind(&plaintext)` 
3. admin key 列表不再显示完整 key

---

## 数据库

SQLite WAL 模式，通过 sqlx 的编译期 migration 自动建表。

### 核心表

| 表 | 用途 | 关键字段 |
|----|------|----------|
| `users` | 用户 | username, role, password_hash (argon2), policy_id, last_seen_at |
| `policies` | 策略模板 | max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd |
| `api_keys` | API Key | user_id, lookup_key, key_hash (blake3), last_used_at, last_used_ip |
| `api_key_account_bindings` | Key↔账号绑定 | api_key_id, account_id |
| `accounts` | Claude 账号 | cookie_blob, max_slots, status, email, account_type |
| `account_runtime_state` | 运行时状态 | reset_time, 4×用量窗口, 5×usage bucket, 4×utilization |
| `request_logs` | 请求日志 | 全字段（token/cost/ttft/duration/error），保留 7 天 |
| `usage_rollups` | 费用汇总 | user_id + period_type(week/month) + period_start → cost_nanousd |
| `models` | 模型列表 | model_id, source(builtin/admin/discovered), enabled |
| `settings` | KV 配置 | key → value（stealth 版本、proxy、session_secret 等） |

### 费用精度

所有金额使用 `nanousd`（1 USD = 10⁹ nanousd）存储，避免浮点精度问题。前端显示时转换为 USD。

### Migration

位于 `migrations/` 目录，sqlx 启动时自动执行。命名规范：`{YYYYMMDD}{seq}_description.sql`。

---

## 前端

React 19 + Mantine 9 + TanStack Query，构建产物输出到 `static/` 目录。

### 构建

```bash
cd frontend && npm ci && npm run build
```

产物约 640KB JS + 210KB CSS（gzip 后 ~225KB）。后续可做路由级拆包优化，当前优先级不高。

### 与后端集成

两种模式：

- **embed-resource** feature（Docker/Release）：`static/` 编译嵌入二进制，`include_dir!` 宏
- **external-resource** feature（开发）：运行时从文件系统读 `static/` 目录

两个 feature 互斥，Cargo.toml `default = ["portable", "external-resource"]`。

---

## 构建与发布

### 本地构建

```bash
cargo build --release                                               # 开发默认 feature
cargo build --release --no-default-features --features embed-resource,xdg    # Docker 风格
cargo build --release --no-default-features --features embed-resource,portable  # Release 风格
```

### release.sh

```bash
./release.sh 0.13.0
```

执行：`cargo update` → `cargo set-version` → `cargo test` → 前端构建 → `cargo check` → git commit + tag + push。

tag push 触发两条 CI：

- **build.yml**：跨平台编译（linux/musl/macOS/windows/android × x86_64/aarch64）→ GitHub Release
- **docker-build.yml**：Docker 构建（amd64 + arm64）→ `ghcr.io/waylon256yhw/clewdr-hub`

### pre-commit hook

`.githooks/pre-commit` 执行 `cargo fmt -- --check`。通过 dev.sh 或手动配置：

```bash
git config core.hooksPath .githooks
```

---

## 已知问题与设计决策

### plaintext_key 明文存储

见[认证体系](#关于-plaintext_key)。这是面向高信任小团队的有意取舍，不是遗漏。

### 默认 admin 密码

未设 `ADMIN_PASSWORD` 时初始密码为 `password`，配合 `must_change_password` 标志强制首次改密。适合本地/内网快速启动，**不要在公网裸露时使用默认密码**。

### 流式 slot 泄漏的边界情况

流式请求通过 `SlotDropGuard`（Drop trait + `tokio::spawn`）在 stream 被 drop 时释放 slot。极端情况下（tokio runtime 已关闭），spawn 可能失败导致 slot 泄漏。实际影响：该账号少一个可用并发，直到下次 reload。对于 3–10 人规模可以接受。

### max_slots 不持久化 inflight

inflight 计数是纯内存状态，进程重启归零。这是正确的——重启意味着所有请求已终止，没有真正的 in-flight。`do_reload()` 保留 inflight 计数是为了处理运行时 admin 操作（增删账号）不丢失正在进行的请求。

### 无 OpenAI 兼容层

有意移除。原版 clewdr 的 OAI 层需要请求/响应格式转换，增加维护成本和出错面。本项目聚焦 Claude Code 场景，直接用 Anthropic Messages API，减少一层抽象。

---

## License

AGPL-3.0
