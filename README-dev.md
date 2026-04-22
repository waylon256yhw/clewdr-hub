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
- [代理与测试](#代理与测试)
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

### 调试配置

常用开发配置写在根目录 `clewdr.toml`，也可通过 `CLEWDR_` 前缀环境变量覆盖。

- `debug_cookie = false`（默认）：
  控制手动 `probe_cookie` 是否额外把原始上游 JSON dump 到 `log/probe-dumps/`。
  关闭时，`request_logs` 只保存 `bootstrap_summary + usage`；
  开启时，日志里会额外出现 `debug_dump_file` / `debug_component_bytes`，便于排查超大 bootstrap 响应。
  推荐只在临时排障时通过 `CLEWDR_DEBUG_COOKIE=true` 或直接修改 `clewdr.toml` 开启，不要做成后台常驻开关。
- `no_fs = false`（默认）：
  遗留的“无文件系统”运行模式。开启后会切到内存 SQLite（`:memory:`），并关闭配置落盘、文件日志、JSON dump 等所有文件写入行为。
  该模式不适合当前后台管理/调试工作流，日常开发与排障不要开启。后续应考虑清理这套遗留语义。

---

## 项目结构

```
src/
├── main.rs                    # 入口：CLI 参数、DB 初始化、启动 HTTP server
├── lib.rs                     # 模块注册
├── config/                    # 配置、常量、AccountSlot 结构体
│   ├── constants.rs           # DB_PATH / CONFIG_PATH / 全局 LazyLock
│   └── cookie.rs              # AccountSlot：账号运行时状态（token、用量、窗口）
├── db/
│   ├── mod.rs                 # init_pool / seed_admin / migrations
│   ├── models.rs              # AuthenticatedUser 等共享类型
│   ├── accounts.rs            # AccountWithRuntime / load_all_accounts / batch_upsert
│   ├── proxies.rs             # ProxyRow / build_proxy_url / update_proxy_test_result
│   ├── queries.rs             # authenticate_api_key / touch_api_key / touch_user
│   ├── api_key.rs             # Key 生成（blake3 哈希 + lookup 前缀）
│   └── billing.rs             # insert_request_log / upsert_usage_rollup / upsert_usage_lifetime_total
├── api/
│   ├── claude_code.rs         # POST /v1/messages 主处理器
│   ├── models.rs              # GET /v1/models
│   ├── health.rs              # GET /health
│   ├── auth.rs                # POST /auth/login, /auth/logout
│   └── admin/                 # /api/admin/* 管理 API（accounts, proxies, users, keys, policies, ops...）
├── middleware/
│   ├── auth.rs                # RequireFlexibleAuth（API Key）/ RequireAdminAuth（session cookie）
│   └── claude/request.rs      # ClaudeCodePreprocess：请求预处理、billing header 注入
├── providers/claude/mod.rs    # ClaudeProvider：构建 ClaudeCodeState 并调用 try_chat
├── claude_code_state/
│   ├── mod.rs                 # ClaudeCodeState：持有账号、client、billing context
│   ├── chat.rs                # try_chat / try_count_tokens / 流式转发 / 用量持久化
│   ├── exchange.rs            # OAuth token 交换（cookie → access_token）
│   ├── organization.rs        # 获取组织 UUID
│   └── probe.rs               # 账号探测（类型、邮箱、用量窗口）
├── services/
│   ├── account_pool.rs        # AccountPoolActor（ractor）：调度、回收、inflight 追踪、脏刷盘
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
├── routes/                    # 页面组件（Dashboard, Ops, Accounts, Proxies, Users, Keys, Logs, Settings）
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
   AccountPoolActor::dispatch() ← 选账号（bound 过滤 → inflight 检查 → round-robin）
   token 交换/刷新              ← cookie/OAuth → access_token
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

4. **调度**（`AccountPoolActor::dispatch()`）：
   - `bound_account_ids` 非空时只从绑定账号中选
   - 检查 `inflight < max_slots`
   - 亲和 key 优先来自官方 Claude Code 透传的 `metadata.user_id` 中的 `_session_`，缺失时回退到 cache-control system blocks 的哈希
   - **Phase A**：moka 亲和性缓存命中且账号仍可用 → 返回同一 `account_id`
   - 缓存账号仅因 `inflight >= max_slots` 满槽时，临时借用其他可用账号（优先 `drain_first`），但不改写缓存
   - **Phase B**：无有效缓存时优先选择 `drain_first = true` 且满足绑定约束、仍有空闲槽的账号
   - **Phase C**：否则 round-robin 取第一个可用账号
   - `inflight += 1`

5. **Token 交换**：检查账号的 access_token 状态（None/Expired/Valid），必要时走 cookie → OAuth code → token 或纯 OAuth refresh 流程。

6. **代理绑定**：如果账号配置了 `proxy_id`，会先解析成账号级 `proxy_url`，后续 Claude 请求、OAuth refresh、OAuth probe 都走这个代理；未绑定则直连。

7. **上游请求**：构建伪装请求头（stealth profile）→ `POST api.anthropic.com/v1/messages`

8. **响应处理**：
   - 非流式：读取完整响应 → 提取 usage → 持久化计费 → 返回
   - 流式：返回 SSE stream → 在 `MessageStop` 事件中异步持久化 usage + 释放 slot
   - 流异常终止（客户端断开）：`SlotDropGuard` 的 `Drop` impl 自动释放 slot

9. **重试**：遇到 429/auth 错误时 `release(Some(reason))` 将账号移入 exhausted 队列 → 换一个账号重试，最多 6 次。

---

## 账号调度器

核心是 `AccountPoolActor`（基于 [ractor](https://github.com/slawlor/ractor) 框架的 actor 模型）。

### 状态

```rust
struct AccountPoolState {
    valid: VecDeque<AccountSlot>,           // 可用队列
    exhausted: HashSet<AccountSlot>,        // 冷却中（429）
    invalid: HashSet<InvalidAccountSlot>,   // 失效（auth error）
    moka: Cache<u64, i64>,                  // 亲和性缓存：affinity hash -> account_id（1h TTL）
    inflight: HashMap<i64, (u32, u32)>,     // account_id → (当前并发, max_slots)
    dirty: HashSet<i64>,                    // 待刷盘的 account_id
    db: SqlitePool,
    probing: HashSet<i64>,                  // 正在探测中的 account_id
    reactivated: HashSet<i64>,              // 本轮被重激活的 account_id
    probe_errors: HashMap<i64, String>,     // 探测失败信息
    drain_first_ids: HashSet<i64>,          // 标记为优先消耗的 account_id
}
```

### 消息类型

| 消息 | 触发方 | 作用 |
|------|--------|------|
| `Request` | 请求处理器 | dispatch 一个账号，inflight++ |
| `Return` | 请求结束 | 回收账号（更新 or 移入 exhausted/invalid） |
| `ReleaseSlot` | 请求结束 | inflight-- |
| `Submit` | admin API | 添加新账号 |
| `CheckReset` | 定时器 (5min) | 检查 exhausted 账号是否可以恢复 |
| `FlushDirty` | 定时器 (15s) | 批量写脏数据到 DB |
| `ReloadFromDb` | admin API | 重新加载（保留 inflight 计数） |
| `ProbeAll` | admin API | 重激活所有 disabled 账号并探测 |
| `ProbeAccounts` | admin API | 探测指定账号列表 |
| `BeginProbe` | 探测流程 | 标记账号进入探测状态 |
| `ClearProbing` | 探测完成 | 清除探测状态标记 |
| `SetProbeError` / `ClearProbeError` | 探测流程 | 记录/清除探测错误信息 |
| `GetProbingIds` / `GetProbeErrors` | admin API | 查询探测状态 |

### 并发槽（max_slots）

每个账号有独立的 inflight 计数器。`dispatch()` 跳过 `inflight >= max_slots` 的账号。释放路径：

- **非流式请求**：handler 中显式调用 `release_slot()`
- **流式请求正常结束**：`MessageStop` 事件的 spawn 中释放，用 `AtomicBool` 防重复
- **流式请求异常终止**：`SlotDropGuard` 的 `Drop` 通过 `tokio::spawn` 异步释放
- **reload**：保留已有 inflight 计数，只更新 max_slots 值

### 脏刷盘

每 15 秒批量 upsert `account_runtime_state` 表（使用 SQLite transaction），只写 dirty set 中的账号。shutdown 时 `post_stop()` 强制刷全量。

### drain_first（优先消耗）

账号级的布尔开关，用于把"限量/试用/促销"账号优先榨干再动主池。行为规则：

- **亲和 key**（`middleware/claude/request.rs::request_affinity_hash`）：
  官方 Claude Code 会透传稳定的 `metadata.user_id`，其中包含 `_session_<uuid>`；调度优先用它作为会话级亲和锚点，保证同一 CLI session 内的 Opus 主请求和 Haiku 辅助请求即使 system prompt / cache-control blocks 不一致，也会落到同一个账号。若请求没有客户端传入的 session metadata（例如 2API 由服务端补注入随机 session），则不使用该随机值，回退到原来的 cache-control system blocks 哈希。
- **Cookie 池**（`services/account_pool.rs::dispatch`）：
  调度先查 moka 亲和性缓存。缓存命中且目标账号满足 `bound` 约束、仍有空闲槽时，继续返回同一个 `account_id`，即使它属于 `drain_first` 池。若缓存账号只是满槽，则临时借用其他可用账号（优先借 `drain_first` 兄弟账号），但不改写缓存；只有缓存账号已失效、被删除或不在本次 `bound` 约束内时才清掉缓存并重新绑定。无有效缓存时才进入 `drain_first` 优先选择，再落回普通 round-robin。
- **OAuth 池**（`providers/claude/mod.rs::OAuthAccountPool::acquire`）：
  将账号**分区**成 drain 和普通两个子集，每个子集维护**独立**的 round-robin 游标（`drain_cursor` / `normal_cursor`）。优先在 drain 子集里按 RR 取可用账号；全部饱和时才在普通子集按 RR 降级。独立游标是为了避免 drain 账号的释放/重取把普通子集的 RR 位置反复拉回头部，造成某一个普通账号（通常是 `rr_order` 最小的）被集中点名。
- **索引重建**：`drain_first_ids` 在每次账号池刷新（`ReloadFromDb` / 启动时 `init`）时从 DB 重建，管理后台勾选后立即生效，无需重启。
- **持久化**：`accounts.drain_first` 列（`INTEGER NOT NULL DEFAULT 0` + CHECK）+ 部分索引 `idx_accounts_drain_first WHERE drain_first = 1`（目前未被 SQL 查询使用，保留为后续潜在批查询做准备）。
- **回收**：冷却（429）或失效（auth error）发生时，和普通账号走同一套 release/exhausted 路径，`drain_first` 标记不影响冷却恢复逻辑。
- **与 `bound` 的关系**：账号池会把 `bound` 集合纳入最终 moka key；命中后仍会校验 `bound`。`bound` 约束先于 `drain_first` 优先级——绑定到普通账号 A 的 API Key 不会被改派到 drain 账号 B。
- **默认值**：`false`；存量部署升级迁移后行为完全不变。

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
| `accounts` | Claude 账号 | cookie_blob, proxy_id, max_slots, drain_first, status, email, account_type |
| `proxies` | 代理资源 | name, protocol, host, port, username, password, last_test_* |
| `account_runtime_state` | 运行时状态 | reset_time, 4×用量窗口, 5×usage bucket, 4×utilization |
| `request_logs` | 请求日志 | 全字段（token/cost/ttft/duration/error/response_body），保留 7 天 |
| `usage_rollups` | 费用汇总 | user_id + period_type(week/month) + period_start → cost_nanousd |
| `usage_lifetime_totals` | 累计汇总 | user_id 维度累计 request/token/cost，独立于日志留存 |
| `models` | 模型列表 | model_id, source(builtin/admin/discovered), enabled |
| `settings` | KV 配置 | key → value（stealth 版本、session_secret 等） |

### 费用精度

所有金额使用 `nanousd`（1 USD = 10⁹ nanousd）存储，避免浮点精度问题。前端显示时转换为 USD。

### Migration

位于 `migrations/` 目录，sqlx 启动时自动执行。命名规范：`{YYYYMMDD}{seq}_description.sql`。

### request_logs 类型

当前 `request_logs.request_type` 受 SQLite `CHECK` 约束限制，允许值包括：

- `messages`
- `probe_cookie`
- `probe_oauth`
- `probe_proxy`
- `test`

新增 probe 类型时，除了改 Rust 枚举 `RequestType`，还必须同步补 migration 更新 `request_logs` 的约束。

### Cookie Probe 调试

手动触发 `probe_cookie` 时，正式日志默认只保存精简后的：

- `bootstrap_summary`
- `usage`

这样可以避免把 `growthbook` / `system_prompts` 一类超大 console bootstrap 原文写进 `request_logs`。

如果要抓原始上游 JSON：

1. 在 `clewdr.toml` 里把 `debug_cookie = true`
   也可以用环境变量：`CLEWDR_DEBUG_COOKIE=true ./dev.sh`
2. 重启后端
3. 从后台手动触发目标账号的 cookie probe
4. 到日志详情查看 `debug_dump_file`
5. 打开 `log/probe-dumps/*.json`

补充说明：

- 只有手动 probe 会带 `debug_dump_file`，自动后台探测默认不写 dump
- 如果 probe 日志本身超过 `PROBE_BODY_MAX_BYTES`，日志行会显示 `truncated=true`，但仍会保留 `debug_dump_file`
- `no_fs = true` 时不会写 dump 文件，因此也不会生成 `debug_dump_file`

---

## 代理与测试

### 代理模型

- 代理已经从旧的全局设置项提升为独立资源，存放在 `proxies` 表。
- 一个实例可同时维护多个备用代理；当前只做“保存多个、按需选择”，不做自动轮换。
- 每个账号可通过 `accounts.proxy_id` 绑定一个代理，也可以留空直连。

### 生效路径

账号级代理会影响这些出站路径：

- Claude Messages 请求
- OAuth callback 交换
- OAuth refresh
- OAuth probe / account test

实现上通过 `load_all_accounts()` 组装 `proxy_url`，再传给 `ClaudeCodeState` / `oauth.rs` 统一构建带代理的 `wreq::Client`。

### 代理测试

管理后台“代理”页的测试是服务器侧的通用连通性测试，不是某个特定上游服务的兼容性测试。当前行为：

- 通过代理请求 `https://ipwho.is/`，失败时回退 `https://httpbin.org/ip`
- 记录延迟、出口 IP、地区信息
- 地区补全通过 `IP2Location.io` 在线查询完成
- 成功/失败结果会持久化到 `proxies.last_test_*`

### 代理测试日志

- 每次代理测试都会写一条 `request_logs`
- 类型为 `probe_proxy`
- `response_body` 保存结构化 JSON bundle，包含：
  - 代理基础信息：仅 `id / name / protocol / host / port`
  - 上游探测尝试列表
  - 地区补全返回
  - 最终测试结果
- 注意：日志 bundle 明确不记录代理用户名、密码，也不记录带凭据的完整代理 URL

---

## 前端

React 19 + Mantine 9 + TanStack Query，构建产物输出到 `static/` 目录。

### 构建

```bash
cd frontend && npm ci && npm run build
```

产物约 640KB JS + 210KB CSS（gzip 后 ~225KB）。后续可做路由级拆包优化，当前优先级不高。

### 运维页数据口径

- 路由：`/ops`
- API：`GET /api/admin/ops/usage?range=24h|7d|30d&top_users=...&user_id=...`
- 累计卡片：来自 `usage_lifetime_totals`（不受 `request_logs` 7 天清理影响）
- 图表窗口：来自 `request_logs`，按 `Asia/Shanghai`（UTC+8）按小时/天分桶
- 自动刷新：`refetchInterval=60_000`，并叠加 `/api/admin/events` 的 SSE invalidation

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

### 发布流程

版本号遵循 semver（`MAJOR.MINOR.PATCH`）。Bug fix → patch，新功能 → minor，破坏性变更 → major。

#### 前置：安装 cargo-edit

`release.sh` 依赖 `cargo set-version`，由 `cargo-edit` 提供。首次在新节点发版前安装一次即可：

```bash
cargo install cargo-edit
```

#### 发版步骤

1. **确认工作区干净**：`git status` 应该只剩需要发版的已 commit 变更，`git log origin/master..HEAD` 看一眼待发布的 commit。

2. **运行 `./release.sh X.Y.Z`**（注意不带 `v` 前缀）：

   ```bash
   ./release.sh 1.0.16
   ```

   脚本依次执行：

   - `cargo update`（刷新 `Cargo.lock` 里的依赖 patch 版本）
   - `cargo set-version X.Y.Z`（同步改 `Cargo.toml` 和 `Cargo.lock` 里的包版本）
   - `cargo test`
   - `cd frontend && npm ci && npm run build`（产物写到 `static/`）
   - `cargo check`
   - `git add Cargo.toml Cargo.lock && git commit -m "Update to vX.Y.Z"`
   - `git push`（推 master）
   - `git tag -a vX.Y.Z -m "Release vX.Y.Z"`
   - `git push origin vX.Y.Z`（推 tag）

   任何一步失败会直接 `set -e` 退出；失败后按下面的「失败恢复」处理。

   Changelog 不需要手动维护——CI 会通过 git-cliff 从 conventional commits 自动生成当前版本的变更日志作为 GitHub Release body。

4. **验证 CI**：

   ```bash
   gh run list --limit 5                 # 看 build / Docker workflow 状态
   gh run watch <run-id>                 # 跟踪某个 run 的实时日志
   gh release view vX.Y.Z                # release 创建成功后可见
   ```

   tag push 会触发两条 workflow：

   - **build.yml**：跨平台编译二进制 → `softprops/action-gh-release@v2` 用整个 `RELEASE_NOTES.md` 作为 body 自动创建 GitHub Release
   - **docker-build.yml**：多架构 Docker 镜像 → `ghcr.io/waylon256yhw/clewdr-hub:vX.Y.Z`

#### 失败恢复

| 情况 | 处理 |
|---|---|
| `cargo test` / 前端构建 / `cargo check` 失败 | 修代码 → `git add` → `git commit --amend` 或新 commit → 重新跑 `./release.sh`。此时 Cargo.toml 里已经是目标版本，`cargo set-version` 幂等，不会冲突。 |
| 脚本跑到一半（已 commit 未 push）失败 | 检查 `git log`，如果「Update to vX.Y.Z」这个 commit 已经存在且内容正确，直接手动执行剩余的 `git push`、`git tag`、`git push origin vX.Y.Z`。 |
| tag 已推送但 CI 构建失败 | 先在 GitHub 上删掉对应 release 和 tag：`gh release delete vX.Y.Z --yes --cleanup-tag`；本地 `git tag -d vX.Y.Z`；修 bug → 补 commit → 重新 `./release.sh X.Y.Z`（版本号不变，因为原 tag 没有任何东西依赖它）。 |
| Release body 内容有误 | Release 创建后，用 `gh release edit vX.Y.Z --notes "corrected content"` 更新 body；不需要重打 tag。 |

#### 设计说明

- **脚本不自建 GitHub Release**：创建动作完全委托给 `build.yml` 里的 `softprops/action-gh-release@v2`，避免本地 `gh release create` 和 CI 并发创建导致冲突。
- **Release body 由 git-cliff 自动生成**：CI 中通过 `orhun/git-cliff-action` 从上一个 tag 到当前 tag 之间的 conventional commits 生成 changelog，只包含当前版本的变更。配置见 `cliff.toml`。
- **版本号同时写在 `Cargo.toml` 和 `Cargo.lock`**：`cargo set-version` 两处都改；手动 bump 时别漏了 `Cargo.lock` 第二处 `[[package]] name = "clewdr-hub"` 的 `version` 字段。

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
