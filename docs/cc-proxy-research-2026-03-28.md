# Claude Code Proxy 项目调研报告

> 日期：2026-03-28
> 调研范围：clewdr、sub2api、claude-cloak 三个项目的 Claude Code 代理实现
> 数据来源：真实 CLI 抓包 + 源码审计

---

## 一、调研对象

| 项目 | 位置 | 语言 | 定位 |
|------|------|------|------|
| **clewdr** | `/root/clewdr` | Rust (Axum) | 轻量个人代理，Cookie→OAuth 自动化 |
| **sub2api** | `/root/sub2api` | Go (Gin) | SaaS 级 API 网关，多平台分发 |
| **claude-cloak** | `/root/claude-cloak` | TypeScript (Bun) | 轻量反代，面向第三方中转站 |

### 抓包数据文件

| 文件 | 内容 | 说明 |
|------|------|------|
| `/root/claude-cloak/docs/max.txt` | Max 订阅 OAuth 直连 `api.anthropic.com` | 真实 CLI v2.1.77，基准参考 |
| `/root/claude-cloak/docs/MICU.txt` | MICU 中转站 `www.openclaudecode.cn` | 客户端 v2.1.81 |
| `/root/claude-cloak/docs/MICU0326.txt` | MICU 中转站更新抓包 | 补充数据 |
| `/root/claude-cloak/docs/yescode.txt` | yescode 中转站 `co.yes.vg` | 客户端 v2.1.77 |

### claude-cloak 已有分析文档

| 文件 | 内容 |
|------|------|
| `/root/claude-cloak/docs/stealth-headers-upgrade-2026-03-12.md` | 伪装头版本升级记录，含逆向分析 |
| `/root/claude-cloak/docs/stealth-deep-analysis-2026-03-18.md` | 深度伪装审计，含三方对比 |
| `/root/claude-cloak/docs/cache-injection-research.md` | cache_control 注入研究 |

---

## 二、架构对比

### 2.1 ClewdR — 双通道代理

核心文件：
- `src/config/constants.rs` — 常量（版本、UA、盐值、Client ID）
- `src/config/cookie.rs` — Cookie 解析与用量追踪
- `src/middleware/claude/request.rs` — 请求预处理（billing header 注入）
- `src/claude_code_state/chat.rs` — Code 通道请求转发
- `src/claude_code_state/exchange.rs` — OAuth2 PKCE 流程
- `src/services/cookie_actor.rs` — Cookie 池管理（ractor actor）

**双通道设计**：

| 通道 | 端点 | 认证 | 转发方式 |
|------|------|------|----------|
| Web | `/v1/messages` | Cookie 直接使用 | 重度转换（Messages→Web 对话格式） |
| Code | `/code/v1/messages` | Cookie→OAuth2 PKCE→Bearer Token | 几乎透传，仅注入 billing header |

**Code 通道请求体修改（最小侵入）**：
1. 在 system prompt 前置 billing header 文本块（计费校验，无语义）
2. 可选插入 custom_system（默认为空）
3. 移除 `cache_control.ephemeral.scope` 字段
4. `-thinking` 后缀→thinking 参数；`-1M` 后缀→1M beta 探测
5. temperature/top_p 互斥处理

**不做的事**：不注入角色提示词，不删除 temperature/tool_choice，不注入空 tools，不伪造 metadata。

### 2.2 Sub2API — SaaS 网关

核心文件：
- `backend/internal/pkg/claude/constants.go` — 硬编码常量和默认头
- `backend/internal/service/identity_service.go` — 指纹管理（Stainless 头缓存、session ID 伪装）
- `backend/internal/service/gateway_service.go` — 网关核心（8000+ 行，CC 伪装在此）
- `backend/internal/service/openai_account_scheduler.go` — 三层调度器
- `backend/internal/service/concurrency_service.go` — Redis 并发槽位
- `backend/internal/service/ratelimit_service.go` — 多维限流

**CC 伪装层问题汇总**：

```go
// constants.go — 过时的默认指纹
var DefaultHeaders = map[string]string{
    "User-Agent":                  "claude-cli/2.1.22 (external, cli)",  // 落后 60+ 版本
    "X-Stainless-Package-Version": "0.70.0",  // 真实值 0.74.0
    "X-Stainless-Runtime-Version": "v24.13.0", // 真实值 v24.3.0
    "X-Stainless-Arch":            "arm64",     // 客户端实际多为 x64
    "Anthropic-Dangerous-Direct-Browser-Access": "true", // CLI 不需要
}
```

```go
// gateway_service.go:48 — 强制注入角色提示
claudeCodeSystemPrompt = "You are Claude Code, Anthropic's official CLI for Claude."
```

`injectClaudeCodePrompt()` 不仅 prepend 一个 system block，还将 banner 重复拼接到下一个 text block 前面，导致同一句话出现 2~3 次。

`normalizeClaudeOAuthRequestBody()` 强制删除 `temperature`、`tool_choice`，注入空 `tools: []` —— 破坏下游用户自定义能力。

**调度/并发控制是其真正优势**：
- 三层调度器：session sticky → hash sticky → EWMA 负载均衡
- Redis Sorted Set 分布式并发槽位 + 等待队列
- 429 精确恢复（解析 `anthropic-ratelimit-unified-*` header）
- 多维配额（RPM/日额/周额/5h 窗口）
- 后台 TokenRefreshService 提前刷新

---

## 三、真实 CLI 流量特征（抓包验证）

### 3.1 HTTP 请求头

```http
User-Agent: claude-cli/2.1.77 (external, cli)
X-Stainless-Arch: x64
X-Stainless-Lang: js
X-Stainless-OS: Windows
X-Stainless-Package-Version: 0.74.0
X-Stainless-Retry-Count: 0
X-Stainless-Runtime: node
X-Stainless-Runtime-Version: v24.3.0
X-Stainless-Timeout: 600
anthropic-dangerous-direct-browser-access: true
anthropic-version: 2023-06-01
x-app: cli
```

**UA 格式**：`claude-cli/X.Y.Z (external, cli)` — 注意是 `claude-cli` 不是 `claude-code`。

### 3.2 anthropic-beta 头

OAuth 直连（max）包含 `oauth-2025-04-20` 和 `context-1m-2025-08-07`，中转站客户端不含 OAuth 相关 beta：

```
# OAuth 直连 (9 个)
claude-code-20250219, oauth-2025-04-20, context-1m-2025-08-07,
interleaved-thinking-2025-05-14, redact-thinking-2026-02-12,
context-management-2025-06-27, prompt-caching-scope-2026-01-05,
advanced-tool-use-2025-11-20, effort-2025-11-24

# 中转站 (4~6 个，无 oauth/context-1m)
claude-code-20250219, interleaved-thinking-2025-05-14,
[redact-thinking-2026-02-12, context-management-2025-06-27,]
prompt-caching-scope-2026-01-05, effort-2025-11-24
```

### 3.3 请求体 system prompt 结构

真实 CLI 生成的 system 数组结构：

```json
"system": [
  {"type":"text", "text":"x-anthropic-billing-header: cc_version=2.1.77.e19; cc_entrypoint=cli; cch=73b45;"},
  {"type":"text", "text":"You are Claude Code, Anthropic's official CLI for Claude.",
   "cache_control":{"type":"ephemeral","ttl":"1h"}},
  {"type":"text", "text":"<完整系统提示词 ~60KB>",
   "cache_control":{"type":"ephemeral","ttl":"1h"}}
]
```

**billing header 的 cch 值是动态的**：每次请求不同（`73b45`、`eb518`、`409e2`），与请求内容相关。

### 3.4 metadata.user_id 格式

```
user_{64位hex}_account_{uuid}_session_{uuid}
```

---

## 四、伪装层对比总结

| 维度 | 真实 CLI | clewdr | sub2api |
|------|----------|--------|---------|
| **UA 产品名** | `claude-cli` | `claude-code` ❌ | `claude-cli` ✅ |
| **UA 后缀** | `(external, cli)` | 无 ❌ | `(external, cli)` ✅ |
| **UA 版本** | `2.1.77~2.1.81` | `2.1.76` ⚠️ | `2.1.22` ❌ |
| **Stainless 头** | 全套 7 个 | 不发送 | 发送但版本过时 |
| **billing header** | system[0] 动态生成 | system[0] 动态生成 ✅ | 过滤掉客户端的，不注入 |
| **"You are CC..."** | system[1] 由 CLI 生成 | **不注入** ✅ | **额外注入 + 重复** ❌ |
| **temperature** | CLI 按需传 | 保留 ✅ | 强制删除 ❌ |
| **tool_choice** | CLI 按需传 | 保留 ✅ | 强制删除 ❌ |
| **tools 字段** | CLI 传完整列表 | 透传 ✅ | 无工具时注入 `[]` ❌ |
| **TLS 指纹** | Node.js 原生 | Chrome136 模拟 ⚠️ | Go net/http |

### 核心判断

- **clewdr 的"最小侵入"透传策略是最安全的**：不伪造不需要的头，不注入多余内容。虽然 Stainless 头缺失，但这不是必需的（中转站场景下 CLI 本身会发送这些头）。
- **sub2api 的伪装过重且过时**：硬编码值与真实 CLI 偏差大，额外注入的 system prompt 和字段删除都是多余的。但其调度/并发控制是生产级的。
- **理想组合**：clewdr 的透传哲学 + sub2api 的调度/并发基础设施。

---

## 五、多用户分发场景评估

### ClewdR 作为上游的限制

| 风险 | 说明 |
|------|------|
| **无 per-cookie 并发锁** | 同一 cookie 可被并发请求同时使用，易触发 429 |
| **429 级联耗尽** | cookie 移入 exhausted 后，池可能迅速清空 |
| **无入站限速** | 完全靠上游 429 回压，burst 期间直接丢弃 |
| **OAuth token refresh 无互斥** | 并发刷新可能导致 refresh_token 失效 |
| **单进程 + 单 IP** | 单点故障 + IP 级封禁风险 |
| **状态纯内存** | 重启丢失所有 token/用量数据 |

### 适用场景

| 场景 | 可行性 |
|------|--------|
| 个人/小团队 (<5 人) | ✅ 3~5 cookie 轮换够用 |
| 中等分发 (10~20 人) | ⚠️ 需 10+ cookie + new-api 侧限速 |
| 大规模分发 (50+) | ❌ 应使用 sub2api 级别的基础设施 |

### Sub2API 的调度优势

- **三层调度**：session sticky → hash sticky → EWMA 负载均衡（评分公式含 priority/load/queue/errorRate/ttft 五个维度）
- **Redis 并发槽位**：`AcquireAccountSlot` / `ReleaseAccountSlot`，超限排队
- **精确 429 恢复**：解析 `anthropic-ratelimit-unified-*` header 获取 resetAt
- **后台 token 刷新**：TokenRefreshService 提前 3 分钟刷新，独立于请求流程

---

## 六、结论与建议

### 如果目标是接入 new-api 分发

**方案 A：小规模，快速验证**
```
用户 → new-api → clewdr (Code 通道) → Anthropic API
```
- new-api 侧设 per-key RPM 限制
- clewdr 配置多个 cookie
- 适合 <10 并发

**方案 B：大规模，生产级**
```
用户 → new-api → sub2api (清理掉多余的伪装逻辑) → Anthropic API
```
- sub2api 管理 token 池和调度
- 需要修正其 CC 伪装层（或完全剥离，让 sub2api 做纯透传 + 调度）

**方案 C：最优组合**
```
用 clewdr 的 Cookie→OAuth 自动化逻辑批量生产 token
→ 导入 sub2api 管理分发
→ sub2api 伪装层参考 clewdr 的透传哲学重写
```

### clewdr 可优化项

1. UA 格式：`claude-code/2.1.76` → `claude-cli/X.Y.Z (external, cli)`
2. billing header 的 `cch` 值：确认真实生成算法，目前 clewdr 硬编码为固定值
3. 可选：支持 Stainless 头透传（当下游 CLI 已经发送时不覆盖）

---

## 七、Source Map 泄露补充调研（2026-03-31）

> 数据来源：v2.1.88 npm 包 source map 还原的完整 TypeScript 源码
> 还原仓库：[ChinaSiro/claude-code-sourcemap](https://github.com/ChinaSiro/claude-code-sourcemap)（4756 文件，1884 个 .ts/.tsx）
> 补充：[vijaychauhanseo 二进制审计](https://vijaychauhanseo.substack.com/p/i-reverse-engineered-claude-code)（v2.1.85 Mach-O 静态分析）

### 7.1 请求头完整画像（源码级确认）

**文件**：`src/services/api/client.ts`

```typescript
const defaultHeaders = {
  'x-app': 'cli',
  'User-Agent': getUserAgent(),          // claude-cli/{version} (external, cli)
  'X-Claude-Code-Session-Id': getSessionId(),  // ← v2.1.86 新增
  // 条件性 headers:
  // 'x-claude-remote-container-id': ...  // 远程容器模式
  // 'x-claude-remote-session-id': ...    // 远程会话模式
  // 'x-client-app': ...                  // SDK 消费者标识
  // 'x-anthropic-additional-protection': 'true'  // 环境变量开启
}
```

**Stainless 头**由 Anthropic SDK 自动注入（不在 client.ts 里），与我们的实现吻合。

**新增 header**：`X-Claude-Code-Session-Id`，在 v2.1.86 引入。值为启动时生成的 UUID，整个 session 内不变。ClewdR 当前未实现——后续需加。

### 7.2 Beta Flags 完整定义（源码级确认）

**文件**：`src/constants/betas.ts`

| 常量名 | 值 | 对外发送？ |
|--------|-----|-----------|
| `CLAUDE_CODE_20250219_BETA_HEADER` | `claude-code-20250219` | ✅ 非 Haiku 模型 |
| `OAUTH_BETA_HEADER` | `oauth-2025-04-20` | ✅ OAuth 订阅者 |
| `CONTEXT_1M_BETA_HEADER` | `context-1m-2025-08-07` | ✅ 1M 上下文模型 |
| `INTERLEAVED_THINKING_BETA_HEADER` | `interleaved-thinking-2025-05-14` | ✅ 支持 ISP 的模型 |
| `REDACT_THINKING_BETA_HEADER` | `redact-thinking-2026-02-12` | ✅ firstParty + 交互模式 |
| `CONTEXT_MANAGEMENT_BETA_HEADER` | `context-management-2025-06-27` | ✅ Claude 4+ |
| `PROMPT_CACHING_SCOPE_BETA_HEADER` | `prompt-caching-scope-2026-01-05` | ✅ firstParty |
| `TOOL_SEARCH_BETA_HEADER_1P` | `advanced-tool-use-2025-11-20` | ✅ firstParty/Foundry |
| `STRUCTURED_OUTPUTS_BETA_HEADER` | `structured-outputs-2025-12-15` | ⚠️ Statsig gate 控制 |
| `EFFORT_BETA_HEADER` | `effort-2025-11-24` | ❌ 定义了但未在 getAllModelBetas 使用 |
| `FAST_MODE_BETA_HEADER` | `fast-mode-2026-02-01` | ❌ 同上 |
| `TASK_BUDGETS_BETA_HEADER` | `task-budgets-2026-03-13` | ❌ 同上 |
| `ADVISOR_BETA_HEADER` | `advisor-tool-2026-03-01` | ❌ 同上 |
| `TOKEN_EFFICIENT_TOOLS_BETA_HEADER` | `token-efficient-tools-2026-03-28` | ❌ ant-only |
| `SUMMARIZE_CONNECTOR_TEXT_BETA_HEADER` | `summarize-connector-text-2026-03-13` | ❌ ant-only + feature flag |
| `CLI_INTERNAL_BETA_HEADER` | `cli-internal-2026-02-09` | ❌ ant-only |
| `AFK_MODE_BETA_HEADER` | `afk-mode-2026-01-31` | ❌ feature flag |
| `WEB_SEARCH_BETA_HEADER` | `web-search-2025-03-05` | ❌ Vertex/Foundry only |
| `TOOL_SEARCH_BETA_HEADER_3P` | `tool-search-tool-2025-10-19` | ❌ Vertex/Bedrock only |

**ClewdR 当前 9 项**与外部 OAuth 用户实际发送的完全吻合。`effort-2025-11-24` 在源码中未被
`getAllModelBetas()` 调用，多余但无害。`structured-outputs-2025-12-15` 由 Statsig gate
`tengu_tool_pear` 控制，暂无法确认对外部用户是否开启——不加。

### 7.3 Beta Flags 组装逻辑

**文件**：`src/utils/betas.ts` → `getAllModelBetas(model)`

按顺序条件添加：
1. `claude-code-20250219` — 非 Haiku
2. `cli-internal-*` — ant 内部员工
3. `oauth-2025-04-20` — OAuth 订阅者
4. `context-1m-*` — 模型支持 1M
5. `interleaved-thinking-*` — 模型支持 ISP
6. `redact-thinking-*` — firstParty + 非 SDK 交互模式 + settings 未开启 showThinkingSummaries
7. `summarize-connector-text-*` — ant-only + feature flag
8. `context-management-*` — firstParty + Claude 4+
9. `structured-outputs-*` — firstParty + Statsig gate + 特定模型
10. `token-efficient-tools-*` — ant-only + Statsig gate
11. `web-search-*` — Vertex/Foundry only
12. `prompt-caching-scope-*` — firstParty
13. 环境变量 `ANTHROPIC_BETAS` 追加

### 7.4 metadata.user_id 生成确认

**文件**：`src/services/api/claude.ts`

```typescript
metadata: {
  user_id: `user_${dba()}_account_${accountUuid}_session_${sessionId}`
}
```

- `dba()` = `getOrCreateUserID()` 生成的设备级匿名 ID（持久化到 `~/.claude.json`）
- `accountUuid` = OAuth 账号的 organization UUID
- `sessionId` = 每次启动随机 UUID

ClewdR 的实现 `user_{HMAC(salt, api_key_id)}_account__session_{uuid}` 格式吻合，
`account_` 部分为空字符串是因为 organization UUID 在代理场景下不应暴露真实值。

### 7.5 Telemetry 与安全机制（不影响代理，仅记录）

源码确认的客户端侧行为（**均不走 API 请求，走 Statsig 侧信道**）：

| 机制 | 说明 | 对代理的影响 |
|------|------|-------------|
| `tengu_sysprompt_block` | 每次请求前上报 system prompt 前 20 字符 + SHA-256 hash | **无**——客户端侧 Statsig 调用，代理不触发 |
| `tengu_off_switch` | 远程 kill switch，OAuth 用户豁免 | **无**——我们走 OAuth |
| `tengu_model_response_keyword_detected` | 客户端侧 sycophancy 检测 | **无** |
| `tengu_session_quality_classification` | 会话质量分类 | **无** |
| IDE 指纹（`clientType`） | 通过环境变量检测 IDE | **无**——服务端不看 |
| `x-client-request-id` | 每个 fetch 请求带随机 UUID | ClewdR 不发送，可选加 |

### 7.6 ClewdR 待办清单（按优先级）

| 优先级 | 改动 | 当前状态 | 说明 |
|--------|------|----------|------|
| **高** | `X-Claude-Code-Session-Id` header | ❌ 缺失 | v2.1.86+ 每个请求携带，缺失是指纹缺陷 |
| **中** | `x-client-request-id` header | ❌ 缺失 | 每个 fetch 带 UUID，firstParty only，可选 |
| **低** | `effort-2025-11-24` 从 beta flags 移除 | 多余但无害 | 源码未在 getAllModelBetas 使用 |
| **低** | `structured-outputs` beta | 待观察 | Statsig gate 控制，外部开启状态未知 |
| **无** | 版本默认值更新 | 不需要 | 已支持 admin 面板动态配置 |
