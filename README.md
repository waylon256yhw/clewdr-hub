<p align="center">
  <img src="assets/logo.svg" width="140" alt="clewdr-hub" />
</p>

# clewdr-hub

基于 [clewdr](https://github.com/Xerxes-2/clewdr) 的多用户 Claude 共享网关。

```
单二进制 / SQLite / 无额外提示词 / 原生 Anthropic Messages API
```

把 Claude Pro/Max 订阅变成团队 API：账号池轮询、并发槽隔离、per-user 限额、费用追踪，开箱即用。

---

## 特性

- **零依赖部署**：单个静态链接二进制，前端编译嵌入，SQLite WAL 自动建库
- **透明代理**：直接转发 `/v1/messages`，不注入系统提示词；仅为兼容 Anthropic 模型行为做最小参数归一化
- **OpenAI 兼容入口**：`POST /v1/chat/completions` + `/v1/models?format=` 协商，零改造对接 OpenAI SDK 客户端，复用同一套鉴权 / 配额 / 限流 / 计费链路
- **轻量伪装**：可配置 CLI/SDK 版本号和请求头，过上游客户端检测
- **多账号调度**：Cookie / OAuth / Custom Anthropic API Key 账号池 + round-robin + 亲和性缓存 + per-account 并发槽（`max_slots`），支持标记账号「优先消耗」用于限量试用账号
- **多代理管理**：可维护多个备用代理，支持账号级绑定，适合不同账号走不同出口
- **团队隔离**：用户 → 策略 → API Key，并发/RPM/周预算/月预算多重限额
- **Per-Key 绑定**：把特定 key 锁定到指定账号，隔离资源
- **管理后台**：总览 / 运维 / 账号池 / 用户 / Key / 日志 / 设置，SSE 实时推送
- **自适应探测**：自动识别 Pro/Max 账号类型，按实际用量窗口显示
- **代理简易测试**：服务器侧测试代理基础连通性、延迟、出口 IP 与地区

## 部署

### 一键安装（推荐）

Linux / macOS / Termux 一条命令装好，自动注册开机自启：

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/install.sh | bash
```

装完直接打开管理菜单：

```bash
clewdr menu
```

按编号选择 — 查看状态、改密码、导入导出配置都在里面。

管理后台地址 `http://你的IP:8484`，默认密码 `password`，首次登录强制改密。

#### Termux 用户

要在关掉 Termux 后服务仍在线，先到 F-Droid 装一次 [Termux:Boot](https://f-droid.org/packages/com.termux.boot/) 并打开它，再执行：

```bash
clewdr service install
```

### Docker Compose

```bash
mkdir clewdr-hub && cd clewdr-hub
curl -O https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/docker-compose.yml
docker compose up -d
```

管理后台：`http://your-ip:8484`，默认密码 `password`，首次登录强制改密。数据持久化在 Docker volume `clewdr-data` 中，`docker compose down` 不会丢数据。

### 手动安装

到 [Releases](https://github.com/waylon256yhw/clewdr-hub/releases/latest) 下载对应平台的 zip，解压加可执行权限即可。要开机自启可以接着跑 `clewdr service install`。

### 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `CLEWDR_IP` | `0.0.0.0` | 监听地址 |
| `CLEWDR_PORT` | `8484` | 监听端口 |
| `ADMIN_PASSWORD` | `password` | 管理员密码（首次登录强制修改） |

## 日常管理

```bash
clewdr menu       # 交互式管理菜单
clewdr            # 一眼看运行状态
clewdr update     # 升级到最新版
```

`clewdr menu` 里能做：查看状态、查看诊断、重置密码、导入/导出配置、安装/卸载服务、检查更新。

## 卸载

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/uninstall.sh | bash
```

默认只删二进制和服务注册，保留你的配置和数据。要把配置和数据库也删掉：

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/uninstall.sh | bash -s -- --purge
```

## 使用

```bash
export ANTHROPIC_BASE_URL=http://your-ip:8484
export ANTHROPIC_API_KEY=sk-...    # 从后台创建
```

流程：**后台登录 →（可选）先到代理页添加备用代理 → 账号池添加 Cookie / OAuth / API Key 账号并按需绑定代理 → 创建 API Key → 客户端配置上面两行**。单人到这里就够了。

### 账号池与 Custom Anthropic API Key

账号池支持三类上游账号：

- **Cookie / OAuth**：面向 Claude Pro / Max 订阅账号，后台会探测账号类型、用量窗口和重置时间。
- **API Key**：面向官方 Anthropic API key 或兼容 Anthropic Messages API 的自定义端点，例如需要 `ANTHROPIC_BASE_URL`、`ANTHROPIC_API_KEY` 和额外请求头的内部 / 云厂商代理。

添加 API Key 账号时填写：

- **基础 URL**：例如 `https://api.anthropic.com/` 或自定义 Anthropic-compatible endpoint；服务端会规范化后拼接 `/v1/messages`、`/v1/messages/count_tokens`。
- **API 密钥**：通过 `x-api-key` 发送；编辑账号时留空表示保留原值。
- **额外请求头**：用于工作区、租户或自定义路由信息，例如 `anthropic-workspace-id`。这些值会在管理员账号编辑页回显，方便维护；导出 `--no-secrets` 和运行日志仍会避免泄露 API key 与 header 值。

API Key 账号不会显示 5h / 7d 订阅用量窗口，也不会参与全量订阅探测。请求日志、用户配额、计费统计、OpenAI 兼容入口仍走同一套链路。

`优先消耗` 适合把临时额度、试用额度或希望先用掉的账号放在调度前面。不要给所有账号都打开，否则等同于没有优先级；也不要把高价值主力账号误标为优先消耗。

### 请求参数兼容策略

- 服务端会统一移除 `top_p` 和 `top_k`
- 如果启用原生 `thinking`（`enabled` / `adaptive`），不符合 Anthropic 要求的 `temperature` 也会被移除
- `claude-opus-4-7` / `claude-opus-4-8`：`thinking.type=enabled` + `budget_tokens` 会被重写为 `{type:"adaptive","display":"summarized"}`；如果请求里没带 `output_config.effort`，服务端会显式补成 `high`。4.7+ 移除了 extended thinking budgets，保持 enabled 在上游会被静默忽略，客户拿不到思考链。
- 后台“设置”页可开启 Opus 专属 `output_config.effort` 覆盖；当前仅对 `claude-opus-4-5` / `claude-opus-4-6` / `claude-opus-4-7` / `claude-opus-4-8`（含 8 位日期后缀）写入所选 effort，其他模型完全透传客户端原始值。
- 如果管理员选择了旧版 Opus 不支持的 effort，服务端会自动映射到兼容等级：`opus-4.7` / `opus-4.8` 支持 `low/medium/high/xhigh/max` 全部五档；`opus-4.6` 会把 `xhigh` 映射到 `max`；`opus-4.5` 会把 `xhigh` / `max` 映射到 `high`。

这是有意的兼容性取舍：对这个项目的目标场景，保留 `temperature` 作为主要采样旋钮已经足够，同时可以减少不同客户端和不同 Claude 模型之间的参数兼容问题。

### Automatic Prompt Caching（per-key 开关）

API Keys 页面每条 key 都有一个**自动缓存**开关：

- **开启时**：服务端会在每个出口请求体顶层注入 `"cache_control": {"type": "ephemeral"}`。Anthropic 服务端会自动把缓存断点放在最后一个可缓存 block 上，并随多轮对话自动前移。命中后只对 prompt 收 0.1x 的 cache-read 价。
- **关闭时**：不注入任何字段，请求按客户端原样透传，行为与之前一致。
- **/v1/messages/count_tokens 路径不会注入**（缓存对它无意义），不用担心。
- **生效阈值**：Sonnet ≥ 1024 token、Opus / Haiku 4.5 ≥ 4096 token 才会真正写入缓存，达不到只是没收益、不会报错。

**适用场景**：上下文稳定增长的多轮对话（聊天、Agent 反复读同一份资料）。
**不要开的场景**：客户端已经自己管理 cache 断点（比如官方 Claude Code，自己会塞 3~4 个，再加一个可能超出 4 个 slot 上限触发 400）；或者经常重写历史 / 插入中间消息的工作流，每改一次都会让 prefix hash 变化命中失败。

第一版 TTL 锁死默认 5 分钟（每次命中重置倒计时），暂不暴露 1h 选项。

### OpenAI 兼容端点

`POST /v1/chat/completions` 接受标准 OpenAI Chat Completions 请求体，内部翻译成 Anthropic Messages 调用同一套上游链路。鉴权、配额、限流、计费、日志都和 `/v1/messages` 共享，所以 `RequestType` 仍记为 `messages`、`/api/admin/logs` 不需要新过滤项。

最小客户端示例（`openai-python`）：

```python
from openai import OpenAI

client = OpenAI(base_url="http://your-host:8484/v1", api_key="sk-...")
resp = client.chat.completions.create(
    model="claude-sonnet-4-6",
    messages=[{"role": "user", "content": "Hi"}],
)
print(resp.choices[0].message.content)
```

实现要点：

- **支持字段**：`messages`（含 `developer` 角色，自动并入 `system`）、`stream` + `stream_options.include_usage`、`tools` / `tool_choice`（含 `required` → Anthropic `any`，`none` 自动剥除工具）、`response_format`（`text` / `json_object` 静默接受，`json_schema` 映射到 `output_format`）、`reasoning_effort`（`low/medium/high`）、`max_tokens` / `max_completion_tokens`、`stop` 字符串或数组（自动截到 4 条）、`temperature`、`user`、字符串型 `metadata`。
- **多模态**：`image_url` 内容部分支持 `data:image/{jpeg|png|gif|webp};base64,...` 数据 URL 和 `http(s)://` URL；其它 MIME / scheme 直接 400。
- **思考链**：上游 `thinking` 内容块默认映射到 OpenAI/DeepSeek 约定的 `message.reasoning_content`；客户端发送 `x-include-reasoning: false`（或 `0` / `no`）可关闭。
- **流式**：响应是 `text/event-stream`，每个 chunk 是 `chat.completion.chunk`。开启 `stream_options.include_usage` 后，`[DONE]` 之前会单独发一个 `choices=[], usage={...}` chunk（含 `prompt_tokens_details.cached_tokens`）。
- **Usage 归并**：`prompt_tokens = input + cache_creation + cache_read`，`cache_read` 单独经 `prompt_tokens_details.cached_tokens` 透出。
- **静默忽略**：`frequency_penalty` / `presence_penalty` / `logit_bias` / `seed` / `service_tier` / `store` / `top_logprobs`、非 string 的 `metadata` 字段、未识别字段；接收但不映射到上游。
- **硬性拒绝**：`n > 1` 与 `logprobs: true` 显式 400（语义无法在 Anthropic 上等价实现，避免静默偏差）。
- **错误体**：所有错误（鉴权失败、配额超限、上游池空、上游 4xx/5xx）都按 `{ "error": { "message", "type", "code", "param" } }` 返回；状态码与 `/v1/messages` 同一信号保持一致。
- **CORS**：preflight 接受 `openai-beta` / `openai-organization` / `openai-project` 三个浏览器侧客户端常用 header。

### `/v1/models` 格式协商

| 查询字符串 | 形态 | 用途 |
| -- | -- | -- |
| 缺省 / `?format=` | 兼容超集（同时含 Anthropic `display_name/created_at/type/has_more/first_id/last_id` 和 OpenAI `object/created/owned_by`） | 老客户端无感升级，默认行为 |
| `?format=openai` | 严格 OpenAI：顶层 `object="list"`，`data[].object="model"`，`created` 是 Unix 秒，`owned_by="anthropic"` | OpenAI SDK / LiteLLM / one-api 风中转 |
| `?format=anthropic` | 当前 Anthropic 形态（无 `object` / `created` / `owned_by`） | 严格按 Anthropic models API 解析的客户端 |
| 其他值 | 400 `invalid_request_error` | 阻止拼写错误静默走默认 |

同一个查询字符串也作用于 `GET /v1/models/{id}` 单条接口。

### Anthropic 1M Context 说明

- 本项目不再支持 legacy `-1M` 伪模型名，请直接使用 Anthropic 官方标准模型名
- 本项目不会主动添加 `context-1m-2025-08-07`，也会忽略客户端传入的这个 legacy beta header
- 依赖 `context-1m-2025-08-07` 的过渡 1M beta 已在 `2026-04-30` 后退出支持范围；需要 1M context 时请使用已经原生支持该能力的官方模型名

### 团队扩展

在上面基础上：

1. **策略**（用户页 → 策略标签）：定义并发/RPM/周月预算模板。周/月预算留空或填 0 表示该周期不限制，策略列表会显示 `∞`
2. **用户**：为成员创建账号，分配策略（管理员账号内置，不可新建）
3. **分发 Key**：每人一个 key，可选绑定到特定账号
4. 超限请求直接拒绝，不消耗账号资源

## 后台功能

地址即服务根路径，管理员登录后可见：

| 页面 | 用途 |
|------|------|
| **总览** | 账号/用户/Key 数量，请求量，当前伪装版本 |
| **运维** | 累计请求/Token/金额，模型分布，用户用量趋势与日志下钻 |
| **账号池** | 添加/管理 Cookie / OAuth / Custom Anthropic API Key 账号，给账号绑定代理，查看订阅账号用量窗口和重置倒计时 |
| **代理** | 维护多个备用代理，测试基础连通性/延迟/出口 IP/地区 |
| **用户** | 成员 CRUD + 策略管理（并发/RPM/预算） |
| **API Keys** | 创建/绑定/管理 Key |
| **日志** | 请求明细，按用户/状态/模型/时间筛选，点击展开详情 |
| **设置** | CLI 版本伪装、模型列表管理、改密 |

### 设置项说明

- **CLI 版本伪装**：从 npm 拉取最新版本号，切换后立即生效。上游更新检测策略时用。
- **模型列表**：控制 `/v1/models` 返回内容，可添加自定义模型 ID。禁用 ≠ 不可调用，只是不列出。

### 代理页说明

- **多个备用代理**：代理是独立资源，可同时保存多条，不做自动轮换；按需为账号绑定。
- **账号级绑定**：每个账号可单独选择一个代理，也可以留空直连。
- **简易测试**：测试在服务器侧执行，用于确认基础连通性；会展示延迟、出口 IP 和地区信息。
- **适用范围**：这是通用代理测试，不代表一定适用于某个具体上游服务。

## 与同类项目对比

|  | **clewdr-hub** | **Sub2API** | **CLIProxyAPI** | **clewdr** (原版) |
|--|---------------|-------------|-----------------|------------------|
| 定位 | 小团队自用网关 | 商业级中转/拼车平台 | 多 provider 代理 | 个人轻代理 |
| 部署 | Rust 单二进制 + SQLite | Go + PostgreSQL + Redis | Go 单二进制 | Rust 单二进制 |
| 支持 provider | Claude 专精 | Claude / OpenAI / Gemini / Antigravity | Gemini / OpenAI / Claude / Codex / Qwen | Claude |
| 代理方式 | cookie → 原生 Messages API | OAuth + cookie | OAuth 包装 CLI | cookie |
| 提示词注入 | **无**，透明转发 | 有平台层注入 | 有 | 无 |
| 用户端 UA 校验 | **不做**，自由接入 | 有 | 有 | 无 |
| 伪装 | 可配版本号 + 请求头 | 内置 | 内置 | 可配版本号 |
| 多用户 | 用户/策略/Key/RBAC | 用户/Key/计费/支付 | 管理 API | 单 admin |
| 管理后台 | 内嵌 7 页 React | Vue 全功能后台 | 社区 Dashboard | 配置页 |
| 适合规模 | 3–10 人 | 10–1000+ 人 / 商用 | 个人–中小团队 | 个人 |
| 资源占用 | ~20MB RAM | PG + Redis + Go | ~50MB RAM | ~15MB RAM |

**如果你是 3–10 人团队共享 Claude 订阅，要轻、要透明、不想运维数据库——这个项目就是为你写的。**

fork 自 [clewdr](https://github.com/Xerxes-2/clewdr)，保留其核心代理能力（轻量伪装、cookie 认证、无提示词注入），重构为多用户网关：

**新增**：用户/策略/RBAC、API Key 认证（blake3）、账号池并发槽调度、请求日志与费用追踪、运维统计与日志下钻、管理后台（7 页）、SSE 实时事件、审计字段

**移除**：`/code/v1/*` 路由。OpenAI 兼容入口已通过 `/v1/chat/completions` 完整重做，详见上方"OpenAI 兼容端点"一节。

## 致谢

[clewdr](https://github.com/Xerxes-2/clewdr) by [Xerxes-2](https://github.com/Xerxes-2)

## License

AGPL-3.0
