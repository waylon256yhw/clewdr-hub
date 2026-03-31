# ClewdR Fork 架构设计文档

> 日期：2026-03-29（rev.3 同日更新，+请求伪装章节 +入站认证基础）
> 更新：2026-03-31（rev.7，Phase 0-4 实现完成，+Phase 4.5 请求伪装）
> 基于：[cc-proxy-research-2026-03-28.md](./cc-proxy-research-2026-03-28.md) 调研结论
> 参与：Claude Code (Opus) + Codex 多轮协商
> Session ID: `019d34c8-74f7-72b1-9c70-568d3f402afc`

---

## 一、项目定位

基于 clewdr 二开，面向 **3-10 人小团队**的 Claude 官方订阅共享网关。

| 维度 | 决策 |
|------|------|
| 上游账号池 | 最多 3 个（OAuth cookie），成本约束 |
| 主要客户端 | Claude Code CLI |
| 维护者 | 1 人（部署者本人） |
| 部署形态 | 单 binary / Docker，单节点，2vCPU/4GB 足够 |

### 核心价值（相比原版 clewdr）

1. **多用户管理**——clewdr 没有用户概念，只有单一 admin 密码
2. **公平调度**——clewdr 无 per-cookie 并发锁，无配额控制
3. **成本计量**——按等效金额追踪用量，与官方订阅行为一致
4. **管理面板**——clewdr 前端仅有基础 cookie/config 管理

---

## 二、技术栈

### 保留

| 组件 | 说明 |
|------|------|
| Rust / Axum | 网关核心，继承 clewdr |
| Tokio | 异步运行时 |
| 嵌入式 SPA | Vite 构建产物编译进 binary |

### 新增

| 组件 | 用途 |
|------|------|
| SQLite (sqlx) | 持久化（WAL 模式） |
| argon2 | admin 密码 hash |
| blake3 | API key hash（快速） |
| rand | 密码/key 随机生成（crypto-safe） |
| Mantine v7 | 前端组件库 |
| TanStack Query v5 | 前端数据获取/缓存 |
| react-router v7 | 前端路由 |

### 移除

| 组件 | 理由 |
|------|------|
| Redis | 单节点不需要 |
| react-i18next | 暂无多语言需求 |
| OAI 兼容层 | 不支持 OpenAI 格式 |

---

## 三、架构设计

### 3.1 总体架构

```
用户 (Claude Code CLI)
  │  Authorization: Bearer <api_key>
  ▼
┌──────────────────────────────────────┐
│  Gateway (单 Rust binary)             │
│                                      │
│  ┌─ Auth ─────────────────────────┐  │
│  │ api_key → user → policy        │  │
│  │ 检查: 禁用? 超额? 并发? RPM?   │  │
│  └────────────────────────────────┘  │
│                                      │
│  ┌─ Scheduler ────────────────────┐  │
│  │ round-robin 扫描健康账号        │  │
│  │ 逐个 try_acquire slot (原子)   │  │
│  │ 全部失败 → 等 2-5s 重试一轮    │  │
│  │ 仍失败 → 拒绝请求              │  │
│  └────────────────────────────────┘  │
│                                      │
│  ┌─ Proxy ────────────────────────┐  │
│  │ 最小侵入透传 (clewdr 哲学)     │  │
│  │ 仅: billing header 注入        │  │
│  │    auth token 替换             │  │
│  │    cache_control.scope 移除    │  │
│  │ 不做: 注入 system prompt       │  │
│  │      删除 temperature 等字段   │  │
│  └────────────────────────────────┘  │
│                                      │
│  ┌─ Billing ──────────────────────┐  │
│  │ 解析 response usage            │  │
│  │ cost = tokens × model_price    │  │
│  │ 累加到 weekly/monthly rollup   │  │
│  └────────────────────────────────┘  │
│                                      │
│  ┌─ Admin Panel ──────────────────┐  │
│  │ 嵌入式 React SPA (Mantine)     │  │
│  │ SSE 实时事件推送               │  │
│  └────────────────────────────────┘  │
│                                      │
│  SQLite (WAL)                        │
└──────────────────────────────────────┘
```

### 3.2 端点结构

| 路径 | 方法 | 说明 |
|------|------|------|
| `/health` | GET | 健康检查（无需认证，返回状态/健康账号数/活跃连接） |
| `/v1/messages` | POST | 主转发端点（Claude Code CLI 默认） |
| `/v1/messages/count_tokens` | POST | token 计数 |
| `/auth/login` | POST | admin 登录（返回 session cookie） |
| `/auth/logout` | POST | admin 登出 |
| `/api/admin/overview` | GET | 服务状态总览 |
| `/api/admin/accounts` | GET/POST/PUT/DELETE | 账号池 CRUD |
| `/api/admin/users` | GET/POST/PUT/DELETE | 用户 CRUD（CASCADE 删 keys + rollups） |
| `/api/admin/keys` | GET/POST/DELETE | API key 管理 |
| `/api/admin/requests` | GET | 请求日志查询 |
| `/api/admin/settings` | GET/POST | 全局设置 |
| `/api/admin/events` | GET (SSE) | 实时事件流 |

### 3.3 请求转发流程

```
GatewayService::proxy_message()
  → authenticate API key (lookup_key 前缀缩小范围 → blake3 验证 hash)
  → load user + policy
  → check: disabled? weekly/monthly budget exceeded?
  → check: user concurrency >= max_concurrent? (tokio Semaphore)
  → check: RPM exceeded? (内存滑动窗口)
  → round-robin scan healthy accounts, try_acquire slot on each (原子选择+获取)
  → 全部 try_acquire 失败 → 等待 2-5s 后重试一轮，仍失败则拒绝
  → refresh OAuth token if needed (per-account Mutex 保护)
  → build stealth headers (§3.4)
  → minimally transform request body (billing header 注入)
  → stream upstream response with backpressure
  → parse usage.input_tokens + usage.output_tokens from final SSE event
  → compute cost_nanousd = input × model_input_price + output × model_output_price
  → INSERT request_log + UPSERT usage_rollup
  → release account Semaphore slot
  → emit SSE event to admin panel
```

### 3.4 请求伪装（OAuth 直连场景）

> 参考：[cc-proxy-research-2026-03-28.md](./cc-proxy-research-2026-03-28.md) §三 抓包数据

我们是 **OAuth 直连代理**（Cookie → OAuth PKCE → Bearer Token → `api.anthropic.com`），
不是中转站。区别在于：中转站场景下 CLI 客户端自己带 Stainless 等指纹头，代理只需透传；
而 OAuth 直连场景下请求**从我们发出**，所有头都需要自己构造。

#### clewdr 现有问题

| 问题 | 现状 | 应为 |
|------|------|------|
| UA 产品名 | `claude-code/2.1.76` | `claude-cli/X.Y.Z (external, cli)` |
| UA 版本 | 硬编码 `2.1.76` | 可配置，跟随 CC 发版更新 |
| Stainless 头 | 完全不发送 | 需要全套 7 个 |
| x-app | 不发送 | `cli` |
| anthropic-beta | 仅 `oauth-2025-04-20` 一项 | OAuth 直连需 9 项 |

#### 目标：完整 HTTP Header 集

```http
# 身份标识
User-Agent: claude-cli/{cc_version} (external, cli)
x-app: cli

# API 版本
anthropic-version: 2023-06-01

# Beta 功能（OAuth 直连完整集，9 项）
anthropic-beta: claude-code-20250219, oauth-2025-04-20, context-1m-2025-08-07,
  interleaved-thinking-2025-05-14, redact-thinking-2026-02-12,
  context-management-2025-06-27, prompt-caching-scope-2026-01-05,
  advanced-tool-use-2025-11-20, effort-2025-11-24

# Stainless SDK 指纹（模拟 Anthropic TS SDK）
X-Stainless-Lang: js
X-Stainless-Package-Version: {sdk_version}
X-Stainless-OS: Linux
X-Stainless-Arch: x64
X-Stainless-Runtime: node
X-Stainless-Runtime-Version: v24.3.0
X-Stainless-Retry-Count: 0
X-Stainless-Timeout: 600

# 认证
Authorization: Bearer {oauth_access_token}
```

#### 请求体伪装（保持 clewdr 最小侵入哲学）

| 操作 | 说明 |
|------|------|
| billing header 注入 | system[0] 插入 `x-anthropic-billing-header: cc_version={version}.{hash}; cc_entrypoint=cli; cch={hash};` |
| `cache_control.ephemeral.scope` 移除 | 上游不接受此字段 |
| **不做** | 不注入 "You are Claude Code..." system prompt（CLI 自己会带）|
| **不做** | 不删除 temperature / tool_choice / tools |
| **不做** | 不注入 metadata.user_id（OAuth 直连不需要） |

#### 版本管理（可配置 + 前端可更新）

硬编码版本是最大的维护痛点——CC 大约每周发版，落后太多可能触发检测。

**存储**：SQLite `settings` 表（KV 结构），以下 key 运行时可改：

| Key | 默认值 | 说明 |
|-----|--------|------|
| `cc_cli_version` | `2.1.80` | CC CLI 版本号，影响 UA + billing header |
| `cc_sdk_version` | `0.74.0` | Anthropic TS SDK 版本，影响 Stainless 头 |
| `cc_beta_flags` | *(上述 9 项)* | anthropic-beta 值，逗号分隔 |

**更新方式**：
1. Admin 面板 Settings 页面直接编辑，保存即生效（无需重启）
2. 前端可展示当前值 + 最新 CC 版本提示（后续可选：定时查 npm `@anthropic-ai/claude-code` 获取最新版本号）

**代码改动**：
- 将 `constants.rs` 中的 `CLAUDE_CODE_VERSION`、`CLAUDE_CODE_USER_AGENT`、`CLAUDE_CODE_BILLING_SALT` 改为从 `AppState.settings` 读取
- `build_stealth_headers()` 新函数：组装完整 header 集，替代当前分散在各处的硬编码
- billing header 生成逻辑保留（算法正确），仅版本号改为动态读取

### 3.5 认证模型

**双轨分离，不用 JWT**：

| 场景 | 方式 | 说明 |
|------|------|------|
| CLI/API | per-user API key | `Authorization: Bearer sk-xxx`，SQLite 存 blake3 hash |
| Admin 面板 | session cookie | 密码登录 → HTTP-only cookie + CSRF |

**Admin session 细节**：
- admin 密码 hash（argon2）存 SQLite `users` 表（role='admin'）
- 首次启动时，若无 admin 用户，从环境变量 `ADMIN_PASSWORD` 读取并创建；未设置则随机生成并打印到 stdout
- Session：HMAC 签名 cookie（`tower-sessions` 或自实现），不存 DB，无状态
- Session 过期：24 小时，可配置
- CSRF：Double Submit Cookie 模式（前端每次请求带 `X-CSRF-Token` header）
- Settings 持久化：运行时可变配置存 SQLite `settings` 表（KV 结构），启动配置走环境变量/YAML

**API key 设计**：
- 格式：`sk-` + 32 字节 cryptographically random（base62 编码），服务端生成
- `lookup_key`：key 的前 8 字符，用于快速查库缩小范围
- `key_hash`：blake3 hash，验证用
- 创建时**只显示一次**完整 key，UI 提供一键复制 + 临时显示/隐藏切换
- 不支持用户自定义 key

**入站认证中间件**（基于 [clewdr#130](https://github.com/Xerxes-2/clewdr/pull/130)）：

CC CLI 使用 `ANTHROPIC_AUTH_TOKEN` 配置认证，发送 `Authorization: Bearer` header；
而其他客户端可能用 `x-api-key` header。clewdr 上游已合并 `RequireFlexibleAuth`
中间件解决此问题——两种 header 都接受，优先 `x-api-key`，fallback `Bearer`。

Phase 2 实现时基于 `RequireFlexibleAuth` 改造：
- 原逻辑：从 header 提取 token → 与 admin 密码比对
- 新逻辑：从 header 提取 token → `lookup_key` 前缀查 `api_keys` 表 → blake3 验证 hash → 关联 user + policy
- 保持两种 header 格式兼容，用户配置 CC CLI 时用 `ANTHROPIC_AUTH_TOKEN=sk-xxx` 即可

### 3.6 多用户模型

**Admin 创建用户 + 生成 key，不做自注册**。

用户收到的只有：
1. 网关 base URL
2. 个人 API key
3. 一行 CLI 配置命令

v1 普通用户**无需 web 登录**，零 UI 交互。

### 3.7 公平调度策略

```yaml
policy:
  max_concurrent: 5          # per-user 并发上限（CC subagent 需要余量）
  rpm_limit: 30              # 防滥用安全阀
  weekly_budget_usd: 50.0    # 周等效金额限制
  monthly_budget_usd: 150.0  # 月等效金额限制
```

**调度逻辑**：round-robin 遍历健康账号，无优先级权重。

**429 恢复机制**（复用 clewdr 已有逻辑）：
- 解析上游 `anthropic-ratelimit-unified-reset` header 或响应体 `resetsAt` 字段
- cookie 移入 exhausted 池，设 `reset_time` 为解析到的时间戳；header 缺失则默认 1 小时
- 后台每 300s 检查 exhausted 池，到期自动恢复到 valid 池
- 重试逻辑自动切换到下一个可用 cookie
- 1M 上下文被拒（429 + 特定消息）单独处理：降级重试，不标记 cookie exhausted
- `auth_error` 状态：OAuth token 失效时进入，触发自动 refresh 尝试；
  refresh 成功 → 恢复 active；失败 → 保持 auth_error，每 5 分钟重试

### 3.8 双层用量追踪

用量追踪分为**账户级**和**用户级**两层，目的不同：

#### 账户级：追踪上游 Anthropic 消费窗口（复用 clewdr 已有）

直接复用 clewdr 的 per-cookie 多窗口 token 桶（内存态），**不需要从 request_logs 聚合**：

| 窗口 | 周期 | 维度 | 对应 Anthropic 限制 |
|------|------|------|-------------------|
| `session_usage` | 5 小时 | 全模型 in/out | 5h spending limit |
| `weekly_usage` | 7 天 | 全模型 in/out | 7d total limit |
| `weekly_sonnet_usage` | 7 天 | Sonnet in/out | 7d Sonnet-only limit |
| `weekly_opus_usage` | 7 天 | Opus in/out | 7d Opus-only limit |
| `lifetime_usage` | 永久 | 全模型 | 统计用 |

窗口边界通过调用上游 `/api/oauth/usage` 获取真实 `resets_at` 时间戳，
比自行计算更准确。窗口到期时自动重置桶计数。

**用途**：调度器判断账号健康度、admin 面板展示用量进度条、预测 429。
**不涉及金额**——这一层纯粹是 token 计数，和 Anthropic 限制保持一致。

<details>
<summary>clewdr 已有实现（代码定位）</summary>

**核心数据结构** (`config/cookie.rs`)：
- `UsageBreakdown` — 6 个 `u64` 字段：`total_input/output`, `sonnet_input/output`, `opus_input/output`
- `ModelFamily` — 枚举 `Sonnet | Opus | Other`
- `CookieStatus` — 包含 5 个 `UsageBreakdown` 实例 + 每窗口 `resets_at` 时间戳 + 三态追踪标志（`Option<bool>`：`None`=未知, `Some(true)`=上游提供, `Some(false)`=上游不追踪）
- `add_and_bucket_usage(input, output, family)` — 按 `ModelFamily` 分桶累加，`saturating_add` 防溢出

**Usage 解析管道** (`claude_code_state/chat.rs`)：
- 流式：`forward_stream_with_usage()` — 每收到 `MessageDelta` SSE 事件，`AtomicU64::fetch_add` 累加 `output_tokens`；`MessageStop` 时 spawn 异步 persist
- 非流式：`extract_usage_from_bytes()` — 解析响应 JSON `usage.input_tokens` / `usage.output_tokens`；fallback 用 tiktoken `o200k_base` 本地估算
- 两者最终都调用 `persist_usage_totals()` → `add_and_bucket_usage()`

**窗口重置（双机制）**：
- 惰性刷新：`persist_usage_totals()` 内先调 `update_cookie_boundaries_if_due()`，检查 `now >= resets_at`，到期则调上游 `GET /api/oauth/usage` 获取新 `resets_at` 并重置桶；网络失败时用本地 fallback（`now + 5h` / `now + 7d`）
- 定时扫描：`cookie_actor.rs` 的 `refresh_usage_windows()` 周期性扫描所有 cookie，用同样逻辑重置到期桶

**模型分类** — `classify_model()` 存在两份重复实现（Code / Web 路径各一份），逻辑为 `model.to_lowercase().contains("opus"|"sonnet")`，仅区分三大类，**不区分版本号**
</details>

#### 用户级：配额控制（新增，SQLite 持久化）

以等效 USD 金额追踪每个用户的消费，存入 `usage_rollups` 表：

**计费**：按等效 USD 金额，内部用 `nanousd`（整数）存储避免浮点精度问题。

| 模型 | Input $/MTok | Output $/MTok | nanousd/token (in) | nanousd/token (out) |
|------|-------------|---------------|--------------------|--------------------|
| Opus 4.6 / 4.5 | $5 | $25 | 5,000 | 25,000 |
| Opus 4.1 / 4.0 | $15 | $75 | 15,000 | 75,000 |
| Sonnet 4.6 / 4.5 / 4.0 | $3 | $15 | 3,000 | 15,000 |
| Haiku 4.5 | $1 | $5 | 1,000 | 5,000 |
| Haiku 3.5 | $0.80 | $4 | 800 | 4,000 |

**Cache-aware 计费**（Phase 4 新增）：

| 操作 | 倍率 | 说明 |
|------|------|------|
| Base input | 1.0x | `input_tokens × input_price` |
| Cache write (5min) | 1.25x | `cache_creation_input_tokens × input_price × 125 / 100` |
| Cache read/hit | 0.10x | `cache_read_input_tokens × input_price × 10 / 100` |
| Output | 1.0x | `output_tokens × output_price` |

Cache 倍率为全局常量（Anthropic 对所有模型统一），不存 model_pricing 表。
1 小时 cache (2x) 与 5 分钟 cache (1.25x) 在上游 API 响应中不区分，统一按 1.25x 计费。

**计费策略**：仅在成功解析到 response 最终 `usage` 时计费，部分流/中断记录但收 $0。
预算为 **soft cap**（小团队互信模型，不做预留/防 gaming 机制）：并发请求可能短暂超额，
这是可接受的——过度防护的复杂度不值得。

**未匹配模型兜底**：若请求中的 `model` 字符串无法归一化到已知 `pricing_key`，
按**最贵模型**（当前为 Opus）计费并记录 warning，避免静默 $0。

**时区规则**：周/月 rollup 边界统一使用 **UTC**。周起始日为周一 00:00 UTC。

**模型归一化**（`normalize_model`）：clewdr 已有的 `classify_model()` 只做粗粒度三分类
（`contains("opus")` / `contains("sonnet")` / Other），对账户级 token 桶足够，但对计费
不够精确（如 Sonnet 3.5 与 Sonnet 4 价格不同）。新增 `normalize_model(raw: &str) -> String`
函数，匹配规则：

```
claude-opus-4*          → "claude-opus-4"
claude-sonnet-4*        → "claude-sonnet-4"
claude-haiku-3*         → "claude-haiku-3.5"
其他                    → 原样返回（命中 model_pricing 兜底逻辑）
```

**插入点**：在 `persist_usage_totals()` 调用 `add_and_bucket_usage()` 之后，
增加一步金额计算：`(input_tokens, output_tokens, model_raw)` → `normalize_model()` →
查 `model_pricing` 表 → 算 `cost_nanousd` → 写 `request_logs` + UPSERT `usage_rollups`。
这是纯增量改动，不修改已有的 token 桶逻辑。

---

## 四、数据模型

### SQLite Schema

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;

-- 策略模板
CREATE TABLE policies (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    max_concurrent INTEGER NOT NULL CHECK (max_concurrent > 0),
    rpm_limit INTEGER NOT NULL CHECK (rpm_limit > 0),
    weekly_budget_nanousd INTEGER NOT NULL CHECK (weekly_budget_nanousd >= 0),
    monthly_budget_nanousd INTEGER NOT NULL CHECK (monthly_budget_nanousd >= 0),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- 用户
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT,
    password_hash TEXT,                   -- argon2id PHC 格式，admin 必填
    role TEXT NOT NULL CHECK (role IN ('admin', 'member')) DEFAULT 'member',
    policy_id INTEGER NOT NULL REFERENCES policies(id),
    disabled_at TEXT,
    last_seen_at TEXT,
    notes TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (role != 'admin' OR password_hash IS NOT NULL)
);

-- API Key（per-user 多 key）
CREATE TABLE api_keys (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    label TEXT,
    lookup_key TEXT NOT NULL UNIQUE,     -- 公开前缀，快速查库
    key_hash BLOB NOT NULL UNIQUE,       -- blake3 hash
    disabled_at TEXT,
    expires_at TEXT,
    last_used_at TEXT,
    last_used_ip TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- 账号池（Cookie/OAuth）
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    rr_order INTEGER NOT NULL UNIQUE,    -- round-robin 顺序
    max_slots INTEGER NOT NULL DEFAULT 5 CHECK (max_slots > 0),
    status TEXT NOT NULL CHECK (
        status IN ('active', 'cooldown', 'auth_error', 'disabled')
    ) DEFAULT 'active',
    cookie_blob BLOB NOT NULL,
    oauth_access_token BLOB,
    oauth_refresh_token BLOB,
    oauth_expires_at TEXT,
    organization_uuid TEXT,
    cooldown_until TEXT,
    cooldown_reason TEXT,
    last_refresh_at TEXT,
    last_used_at TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- 模型价格表（每模型一行，价格变动直接 UPDATE，历史价格已快照在 request_logs）
CREATE TABLE model_pricing (
    id INTEGER PRIMARY KEY,
    pricing_key TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    input_nanousd_per_token INTEGER NOT NULL CHECK (input_nanousd_per_token >= 0),
    output_nanousd_per_token INTEGER NOT NULL CHECK (output_nanousd_per_token >= 0),
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- 请求日志（含价格快照，历史成本不受价格更新影响）
CREATE TABLE request_logs (
    id INTEGER PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    request_type TEXT NOT NULL CHECK (request_type IN ('messages', 'count_tokens')),
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    api_key_id INTEGER REFERENCES api_keys(id) ON DELETE SET NULL,
    account_id INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    model_raw TEXT NOT NULL,
    model_normalized TEXT,
    stream INTEGER NOT NULL DEFAULT 1 CHECK (stream IN (0, 1)),
    started_at TEXT NOT NULL,
    completed_at TEXT,
    duration_ms INTEGER,
    status TEXT NOT NULL CHECK (
        status IN (
            'ok', 'auth_rejected', 'quota_rejected',
            'user_concurrency_rejected', 'rpm_rejected',
            'no_account_available', 'upstream_error', 'client_abort'
        )
    ),
    http_status INTEGER,
    upstream_request_id TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    priced_input_nanousd_per_token INTEGER,
    priced_output_nanousd_per_token INTEGER,
    cost_nanousd INTEGER NOT NULL DEFAULT 0,
    error_code TEXT,
    error_message TEXT,
    rate_limit_reset_at TEXT
);

-- 用量汇总（UPSERT 更新）
CREATE TABLE usage_rollups (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    period_type TEXT NOT NULL CHECK (period_type IN ('week', 'month')),
    period_start TEXT NOT NULL,
    period_end TEXT NOT NULL,
    request_count INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cost_nanousd INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (user_id, period_type, period_start)
);

-- 运行时可变设置（KV 存储）
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- 索引
CREATE INDEX idx_users_policy_id ON users(policy_id);
CREATE INDEX idx_api_keys_user_id ON api_keys(user_id);
CREATE INDEX idx_accounts_status_rr ON accounts(status, rr_order);
CREATE INDEX idx_request_logs_user_started ON request_logs(user_id, started_at DESC);
CREATE INDEX idx_request_logs_account_started ON request_logs(account_id, started_at DESC);
CREATE INDEX idx_request_logs_status_started ON request_logs(status, started_at DESC);
CREATE INDEX idx_usage_rollups_user_period ON usage_rollups(user_id, period_type, period_start DESC);
-- model_pricing.pricing_key 已有 UNIQUE 约束，无需额外索引
```

### Seed Data

```sql
INSERT INTO policies (name, max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd)
VALUES ('default', 5, 30, 50000000000, 150000000000);
-- $50/week, $150/month

INSERT INTO model_pricing (pricing_key, display_name, input_nanousd_per_token, output_nanousd_per_token)
VALUES
    ('claude-opus-4-6',   'Claude Opus 4.6',   5000,  25000),
    ('claude-opus-4-5',   'Claude Opus 4.5',   5000,  25000),
    ('claude-opus-4-1',   'Claude Opus 4.1',   15000, 75000),
    ('claude-opus-4-0',   'Claude Opus 4.0',   15000, 75000),
    ('claude-sonnet-4-6', 'Claude Sonnet 4.6', 3000,  15000),
    ('claude-sonnet-4-5', 'Claude Sonnet 4.5', 3000,  15000),
    ('claude-sonnet-4-0', 'Claude Sonnet 4.0', 3000,  15000),
    ('claude-haiku-4-5',  'Claude Haiku 4.5',  1000,  5000),
    ('claude-haiku-3-5',  'Claude Haiku 3.5',  800,   4000);
-- 未匹配模型按最贵价格（Opus 4.0/4.1）兜底计费
```

---

## 五、模块布局

初期保持扁平，领域稳定后再拆分。合并 models+repos 为 `db/`，压缩 scheduler 为少量文件。

```
src/
  main.rs
  lib.rs
  app_state.rs                    -- 全局状态（DB pool, scheduler handle 等）
  router.rs                       -- 路由注册
  error.rs                        -- 错误类型

  config/
    mod.rs
    settings.rs                   -- 环境变量/YAML 配置

  db/                             -- 数据层：模型 + 查询 + 迁移
    mod.rs
    sqlite.rs                     -- 连接池 + 初始化 + 日志清理定时器
    models.rs                     -- 所有表的 Rust struct（sqlx::FromRow）
    queries.rs                    -- 所有 CRUD 查询函数
    migrations/                   -- SQL 迁移文件

  auth/
    mod.rs
    admin_session.rs              -- admin 密码登录 + HMAC 签名 cookie
    api_key_auth.rs               -- API key 认证中间件
    password.rs                   -- argon2 hash

  claude/                         -- 上游 Claude 交互（从 claude_code_state 重构）
    mod.rs
    exchange.rs                   -- Cookie → OAuth2 PKCE（保留）
    client.rs                     -- HTTP 客户端构建
    headers.rs                    -- 请求头处理
    request_transform.rs          -- 最小侵入 body 修改（保留）
    response_usage.rs             -- 解析 SSE 中的 usage 数据
    streaming.rs                  -- 流式转发 + backpressure

  scheduler/
    mod.rs                        -- AccountPool + round-robin + try_acquire
    cooldown.rs                   -- 429 冷却管理 + auth_error 自动恢复
    lease.rs                      -- AccountLease（持有 slot，Drop 释放）
    refresh.rs                    -- 后台 token 刷新（per-account Mutex）

  services/
    mod.rs
    gateway.rs                    -- 核心转发编排 (proxy_message 方法)
    admin.rs                      -- 用户/key/账号/设置 业务逻辑
    usage_meter.rs                -- cost 计算 + rollup 写入
    admin_events.rs               -- SSE 事件广播

  handlers/                       -- 薄 handler，参数提取 + 调 service
    mod.rs
    messages.rs                   -- POST /v1/messages + count_tokens
    admin/
      mod.rs                      -- login/logout + overview
      users.rs                    -- 用户管理
      api_keys.rs                 -- key 管理
      accounts.rs                 -- 账号池管理
      requests.rs                 -- 日志查询
      settings.rs                 -- 全局设置
      events.rs                   -- SSE 端点

  middleware/
    mod.rs
    request_id.rs                 -- 请求 ID 生成
    trace.rs                      -- tracing 集成
```

### 关键边界

- `handlers/*` 薄层，只做参数提取
- `services/gateway.rs` 拥有完整转发生命周期
- `scheduler/` 管账号选择、冷却、并发（RPM 守卫在 gateway 内联实现，无需独立模块）
- `db/queries.rs` 纯 SQLite 访问，无业务逻辑
- `claude/*` 封装所有上游交互
- 初期文件少时可进一步合并，**不要过早拆分**

---

## 六、前端设计

### 技术栈

| 组件 | 选择 | 理由 |
|------|------|------|
| UI 库 | Mantine v7 | 单维护者做 admin 面板，开箱即用最省力 |
| 数据获取 | TanStack Query v5 | 缓存/重验证/乐观更新 |
| 路由 | react-router v7 | 深链接 + 刷新安全 |
| 状态 | local state + auth context | 不引入 Zustand/Redux |
| i18n | 移除 | 暂无真实需求 |

### 路由结构

```
/login                          -- admin 登录
/admin                          -- Overview（服务健康、活跃请求、近期错误）
/admin/accounts                 -- 账号池管理（状态、冷却、负载、token 过期）
/admin/users                    -- 用户 CRUD + 策略分配 + 用量查看
/admin/keys                     -- API key 生成/撤销/轮换
/admin/requests                 -- 请求日志 + SSE 实时流 + 筛选
/admin/settings                 -- 全局设置
```

### 普通用户

v1 **不做**用户端 UI。后续可选加只读页面（用量/余额/key 轮换）。

---

## 七、改造阶段

单维护者场景，Phase 1-4 都触碰同一条请求热路径，应**严格串行**推进：

| Phase | 内容 | 风险 | 依赖 | 状态 |
|-------|------|------|------|------|
| **0** | Fork + 清理（删 Web/OAI，单 endpoint 跑通） | 低 | 无 | ✅ 完成 |
| **1** | SQLite 集成 + 数据模型 + migrations + admin 初始化 | 低 | Phase 0 | ✅ 完成 |
| **2** | 多用户认证（API key 链路 + admin session cookie） | 中 | Phase 1 | ✅ 完成 |
| **3** | Per-user 速率限制（并发 Semaphore + RPM 滑动窗口） | 中 | Phase 2 | ✅ 完成 |
| **4** | 计费系统（cache-aware billing + 配额 + 日志轮转） | 中 | Phase 3 | ✅ 完成 |
| **4.5** | 请求伪装完善（Stainless 头 + UA + beta flags + 动态版本） | 低 | Phase 4 | |
| **5** | Admin API 完善（用户/key/账号/设置 CRUD） | 中 | Phase 4 | |
| **6** | 前端重写（admin panel） | 中 | Phase 5 的 API | |
| **7** | SSE 实时事件 + 日志流 | 低 | Phase 6 | |

每个 Phase 完成后应有可运行的中间状态——Phase 0-3 完成时已经是一个功能可用的多用户网关（只是没有 admin UI）。

### Phase 细节

#### Phase 0: Fork + 清理 ✅

> 已完成：commit `4883772`，-2289/+61 行，24 文件

目标：删除不需要的路径，保留 Claude Code 单 endpoint 可跑通。

**删除**：
- `claude_web_state/` 整个模块（含重复的 `classify_model()`, `persist_usage_totals()`）
- `types/claude_web/` — Web 格式转换
- OAI 兼容层相关代码（`claude2oai.rs`、`response.rs`、`stop_sequences.rs`、`types/oai.rs`）
- 前端 i18n 相关
- 现有前端（后续 Phase 6 用 Mantine 重写）

**简化**：
- `ClaudeContext`：从 Web/Code 双变体枚举简化为单一 struct
- `ClaudeProviders`：移除 `ClaudeWebProvider`，仅保留 Code
- `ClaudeApiFormat` 枚举：完全移除（始终为 Claude 格式）
- 路由：`/code/v1/messages` → `/v1/messages`（匹配上游 Anthropic API 路径）
- `SUPER_CLIENT`：从 `claude_web_state` 迁移到 `claude_code_state`
- `try_count_tokens`：移除 `for_web` 参数及相关死分支

**保留**：
- `claude_code_state/chat.rs` — 完整的 usage 解析管道（`forward_stream_with_usage`, `extract_usage_from_bytes`, `persist_usage_totals`）
- `config/cookie.rs` — `UsageBreakdown`, `ModelFamily`, `CookieStatus`, `add_and_bucket_usage()`
- `services/cookie_actor.rs` — 定时窗口重置
- `api/misc.rs` 中的 `fetch_usage_percent()` — admin 面板需要
- Cookie/OAuth 交换、请求头处理、流式转发等核心管道

**验证**：清理后 `cargo build` 通过，单 cookie 单用户 `/v1/messages` 端到端转发正常。

#### Phase 1: SQLite 集成 ✅

> 已完成：commit `a47e3b2`，+1099 行，10 文件

- 引入 `sqlx 0.8` + SQLite WAL 模式（`SqliteConnectOptions` 配置 PRAGMA，非 migration SQL）
- 建表迁移（`migrations/20260330000001_initial_schema.sql`）
- Seed data（default policy + 3 model_pricing 行）
- Admin 用户初始化（`ADMIN_PASSWORD` 环境变量 → argon2id hash；未设则随机 16 字符打印 stdout）
- 启动时自动 `sqlx::migrate!().run()` — 使用运行时查询（`sqlx::query`），不用编译时检查宏

**实现决策**（与原始设计的偏差）：

| 决策 | 原设计 | 实际实现 | 理由 |
|------|--------|----------|------|
| Schema 查询 | 未指定 | 运行时 `sqlx::query` / `query_as` | schema 仍在变动，编译时检查增加构建摩擦 |
| PRAGMA 位置 | migration SQL | `SqliteConnectOptions::pragma()` | migration 内 PRAGMA 是 session-scoped，不可靠 |
| AppState | 统一 AppState struct | Phase 1 仅存 RouterBuilder | 当前无路由消费 DB，Phase 2 引入 `FromRef<AppState>` |
| accounts 表 | §4 schema 含 accounts | Phase 1 延迟创建 | cookie 系统仍在内存，空表是假数据源（Phase 3 添加） |
| settings 表 | §4 遗漏 | Phase 1 已创建 | 版本管理等功能依赖 KV 存储 |
| users.password_hash | §4 遗漏 | 已补充（TEXT，admin 必填 CHECK） | admin 登录需要密码 hash |
| `:memory:` 模式 | 未考虑 | `max_connections(1)` 防多连接隔离 | SQLite `:memory:` per-connection 独立 |
| admin seed | 未指定原子性 | `INSERT OR IGNORE` + COUNT 检查 | 幂等防并发 |
| DB 路径 | 未指定 | `DB_PATH` 静态 + `--db` CLI 参数 | 复用 `CONFIG_PATH` / `LOG_DIR` 模式 |

#### Phase 2: 多用户认证 ✅

> 已完成：commit `3e39e47`，+423/-208 行，12 文件

- **AppState 统一**：引入 `AppState { db, cookie_actor, code_provider, auth: AuthState }`，
  使用 Axum `FromRef` 让 handler 提取子状态，替代 per-route `.with_state()` 模式
- **db 模块扩展**：`src/db.rs` → `src/db/` 目录（`mod.rs`, `models.rs`, `queries.rs`, `api_key.rs`）
- **API key 生成/验证**：`sk-` + 40 字符 base62，`lookup_key` = body 前 8 字符（UNIQUE），blake3 hash
- **入站认证中间件**：`RequireFlexibleAuth` / `RequireAdminAuth` 使用 `from_extractor_with_state` 获取
  `AuthState`，查 DB API key → fallback 到 `CLEWDR_CONFIG` legacy 密码
- **Handler 清理**：移除 8 个 admin handler 中重复的 `AuthBearer` + `admin_auth` 检查
- **Admin bootstrap**：`seed_admin()` 在新建和升级场景下都自动生成 API key 并打印
- 移除未使用的 `RequireBearerAuth`

**实现决策**（与原始设计的偏差）：

| 决策 | 原设计 | 实际实现 | 理由 |
|------|--------|----------|------|
| Admin session | HMAC cookie + CSRF | 延迟到 Phase 6 | 现有前端用 Bearer/localStorage，Phase 6 用 Mantine 重写时再引入 cookie session |
| Auth middleware 状态 | `from_extractor` | `from_extractor_with_state` | `from_extractor` 层在 `.with_state()` 之前应用，看到的 state 是 `()`，无法满足 `AuthState: FromRef<S>` 约束 |
| Legacy fallback | 未明确 | 保留 `CLEWDR_CONFIG.password` / `admin_password` 双 fallback | 现有前端和单用户部署仍需兼容 |
| DB 错误处理 | 未指定 | `match` + `warn!` 日志，fallback 到 legacy | 静默吃掉 `sqlx::Error` 会隐藏运维问题（Codex 审阅反馈） |
| API key bootstrap | 仅新 admin | 新建 + 升级都检查 | 已有 DB 升级到 Phase 2 时也需要生成 key（Codex 审阅反馈） |
| `AuthenticatedUser` | 未指定存放方式 | 注入 request extensions | 仅 DB API key 路径注入；legacy 密码路径不注入（后续 Phase 将收紧） |

#### Phase 3: Per-user 速率限制 ✅

> 已完成：commit `fa35095`，+214 行，11 文件

clewdr 已有 `CookieStatus` 完整的窗口管理和 cooldown 逻辑，此阶段在外层包装 per-user 公平控制：
- **accounts 表迁移**：`migrations/20260330000002_add_accounts.sql`，创建 accounts 表 + `request_logs.account_id` 列（表为空，Phase 5 Admin API 填充）
- **per-user 并发控制**：`UserLimiterMap`（`RwLock<HashMap<UserId, Arc<UserLimiter>>>`），每用户 tokio Semaphore，从 policy 读 `max_concurrent`
- **per-user RPM 滑动窗口**：每用户 `VecDeque<Instant>` behind `std::sync::Mutex`，60s 窗口，懒清理
- **UserPermit**：`Arc<OwnedSemaphorePermit>` 存入 `response.extensions()`，流式响应结束时才释放
- **ClaudeContext 扩展**：携带 `user_id`/`api_key_id`/`max_concurrent`/`rpm_limit`
- **Auth query 优化**：`authenticate_api_key()` JOIN `policies` 表，一次查出 limits
- **429 错误**：`UserConcurrencyExceeded`/`RpmExceeded`，使用 Anthropic `rate_limit_error` 格式

**不动**：`CookieStatus` 内部的 token 桶、窗口重置、cooldown 定时器——这些原封保留。

**实现决策**（与原始设计的偏差）：

| 决策 | 原设计 | 实际实现 | 理由 |
|------|--------|----------|------|
| AccountPool | per-account Semaphore + round-robin try_acquire | 延迟 | CookieActor 已有 429→exhausted→reset 机制，1-3 账号场景 per-account 并发控制收益低 |
| AccountLease | RAII guard | 延迟 | 无 per-account Semaphore 则不需要 |
| Busy retry | 全部 busy → 等 2-5s | 延迟 | CookieActor 的 429 重试已覆盖 |
| UserLimiterMap | DashMap | `RwLock<HashMap>` | 3-10 用户，DashMap 过度设计 |
| Policy 变更 | 未指定 | 整个 UserLimiter 替换 | 变更频率极低（月级），短暂超限可接受（Codex 审阅建议保留） |
| RPM/并发顺序 | 未指定 | 先并发 permit 再 RPM 记录 | Codex 审阅反馈：被拒请求不应消耗 RPM 配额 |
| 429 格式 | 未指定 | Anthropic `rate_limit_error` 类型 | Codex 审阅反馈：CLI 期望标准格式 |
| Legacy auth | 未明确 | context 字段 None → 不限制 | 向后兼容，现有前端/单用户部署不受影响 |

#### Phase 4: 计费系统 ✅

> 已完成：commit `2070cd8`，+660/-48 行，19 文件（含 4 新文件）

在已有的 token 解析管道上接金额层，同时引入 cache-aware 计费：

**新建文件**：
- `migrations/20260331000001_billing_phase4.sql` — schema 变更 + 9 行模型定价
- `src/billing.rs` — normalize_model、BillingUsage、cost 计算、persist_billing_to_db、check_quota
- `src/db/billing.rs` — 计费相关 DB 查询（request_log INSERT、rollup UPSERT、pricing lookup、log rotation）
- `src/services/log_rotation.rs` — 启动 + 每日定时清理

**核心改动**：

1. **Cache-aware 计费**：扩展 `Usage`/`StreamUsage` 增加 `cache_creation_input_tokens` / `cache_read_input_tokens`
2. **流式路径重构**：新增 `MessageStart` 解析获取上游权威 input/cache 数据；`MessageDelta` 使用 `store`（累积值）而非 `fetch_add`
3. **9 模型定价表**：per-version 粒度（Opus 4.0/4.1 = $15/$75 vs 4.5/4.6 = $5/$25）
4. **normalize_model()**：canonical alias 匹配 + date suffix 支持，fallback 最贵模型
5. **配额检查（soft cap）**：rate limiter 之前查 rollup，超额返回 429
6. **日志轮转**：启动 + 每 24h，retention 从 settings 表读取

**计费公式（纯整数运算）**：
```
cost = input_tokens × input_price
     + cache_creation_tokens × input_price × 125 / 100
     + cache_read_tokens × input_price × 10 / 100
     + output_tokens × output_price
```

**实现决策（与原始设计的偏差）**：

| 决策 | 原设计 | 实际实现 | 理由 |
|------|--------|----------|------|
| Cache 计费 | 未包含 | cache_creation 1.25x + cache_read 0.10x | Anthropic 官方定价，Claude Code 大量使用缓存 |
| Cache TTL 区分 | 未考虑 | 统一 1.25x（5min） | 上游 API 不区分 5min/1h，CLI 只用 ephemeral |
| 流式 output_tokens | fetch_add | store（累积值） | Codex 审阅发现：Anthropic 文档明确 message_delta.usage 是累积值 |
| MessageStart 解析 | 未涉及 | 从 message_start 获取 input/cache 权威值 | Codex 审阅发现：原代码用本地预估 input 而非上游实际值 |
| message_delta cache | 未涉及 | 也读取 input/cache 字段 | message_delta 携带最终权威值 |
| billing hook 持久性 | 未明确 | 非流式 await；流式 tokio::spawn | 与 cookie persist 同级，soft-cap 可接受 |
| quota DB 错误 | 未明确 | fail-open + warn 日志 | 小团队信任模型，DB 故障不应阻断请求 |
| 时间戳格式 | SQLite datetime() | RFC3339 统一 | Codex 审阅：避免 datetime() vs RFC3339 字符串比较不一致 |

#### Phase 4.5: 请求伪装完善

clewdr 已有的伪装基础（billing header 注入、cache_control.scope 移除、OAuth PKCE 流程），
但指纹完整度不足。此阶段补齐所有 HTTP header 指纹 + 动态版本管理：

**已有（clewdr 原版）**：
- ✅ OAuth PKCE 流程（cookie → token → Bearer auth）
- ✅ billing header 注入到 system[0]
- ✅ cache_control.ephemeral.scope 移除
- ✅ anthropic-version: 2023-06-01

**需要补齐**：

1. **User-Agent 格式修正**：`claude-code/2.1.76` → `claude-cli/{cc_cli_version} (external, cli)`
2. **x-app header**：新增 `cli`
3. **anthropic-beta 完整化**：从 1-2 项扩展到 9 项完整列表
4. **Stainless SDK 指纹头（7 个）**：`X-Stainless-Lang/Package-Version/OS/Arch/Runtime/Runtime-Version/Retry-Count/Timeout`
5. **动态版本管理**：从 DB settings 表读取 `cc_cli_version`、`cc_sdk_version`、`cc_beta_flags`，前端可更新

**代码改动**：
- `build_stealth_headers()` 新函数：组装完整 header 集
- `constants.rs` 中的硬编码版本改为从 `AppState.settings` / DB 读取
- settings 表新增 seed：`cc_cli_version`、`cc_sdk_version`、`cc_beta_flags`

**工作量**：约 100 行，低风险。

---

## 八、注意事项

### 架构约束

1. **单节点限制**：SQLite + 内存 Semaphore/RPM 不支持多实例部署，需在文档明确标注
2. **token refresh 互斥**：per-account `tokio::sync::Mutex` 保护，防止并发刷新导致 refresh_token 失效
3. **不存请求体**：日志只记元数据/token 数/模型/错误，不记 body
4. **短暂等待**：所有账号忙时等 2-5s 再拒绝，改善 CC 突发流量体验
5. **价格快照**：request_logs 内嵌当时单价，历史账单不受后续价格更新影响
6. **count_tokens 不计费**：记录但 cost = 0
7. **硬删除安全**：request_logs FK 设为 `ON DELETE SET NULL`，删除用户/账号/key 不影响历史日志（显示为"已删除"），
   且日志本身 7 天轮转

### 运维细节

8. **日志轮转**：`request_logs` 默认保留 7 天（可配置），启动时 + 每日定时执行清理
9. **密钥存储**：cookie_blob、OAuth token 在 SQLite 中为明文存储——服务器已有 root 权限保护，
   at-rest 加密收益低。部署文档中标注此风险即可
10. **Admin 初始化**：首次启动从 `ADMIN_PASSWORD` 环境变量创建 admin 用户；
    未设置则随机生成 16 字符密码并打印到 stdout，之后可通过面板修改
11. **API key 生成**：服务端生成 `sk-` + 32 字节 crypto random（base62），不支持用户自定义
