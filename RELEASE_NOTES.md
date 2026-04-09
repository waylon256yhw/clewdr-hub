# Release Notes

## v1.0.11

### 修复

- 修正流式请求「首字耗时」(TTFT) 测量：原算法零点设在上游响应头抵达 clewdr 之后才起算，对开启响应缓冲的反代上游（nginx `proxy_buffering on`、Cloudflare、各类中转）会被低估到只有几百毫秒。现在改为复用中间件入口处的 `started_at` 作为零点，得到真正的端到端首字延迟，同时包含 cookie 选择 / token 刷新 / 上游握手等 clewdr 自身开销。

## v1.0.10

### 变更

- 移除服务端内部的 legacy 1M context 兼容逻辑，不再根据 `-1M` 伪模型名或账号运行时状态自动切换 long-context 行为。
- 请求头合并策略继续保留通用 `anthropic-beta` 透传，但会忽略已废弃的 `context-1m-2025-08-07` legacy token。
- README 补充 Anthropic 1M context 时间线说明，明确 `2026-04-30` 为旧 Sonnet 1M beta 退场时间点。

### 修复

- 恢复 `/v1/messages/count_tokens` 在上游返回 `403` 时的本地降级缓存，避免同一账号后续重复硬失败。
- 清理 cookie / OAuth runtime 中默认标记“支持 1M”的过时状态，避免与 Anthropic 当前模型能力语义混淆。

## v1.0.9

### 新增

- 管理后台手动点击 "Probe All" 后，每个被探测的账号会写入一条 `request_logs` 行（类型 `probe_cookie` / `probe_oauth`），并把上游 bootstrap / profile / usage 原始 JSON 聚合为一个 bundle 存入新的 `response_body` 列，供日志详情抽屉展开查看。
- 请求日志详情抽屉新增 "上游响应 JSON" 区块，按需懒加载 `GET /api/admin/requests/{id}/response_body`，列表接口自身不再下发响应体。
- 日志表格 Token 列改为四个彩色 Mantine Badge：`↑input` (cyan) / `↓output` (teal) / `+cache_write` (grape) / `↻cache_read` (gray)；缓存 badge 仅在非零时渲染。

### 变更

- `request_logs.request_type` 枚举替换：移除 `count_tokens`，新增 `probe_cookie` 和 `probe_oauth`。`/v1/messages/count_tokens` 端点仍然可用，但不再写请求日志（生产库此前 607 条 `messages` : 0 条 `count_tokens`，该类型纯属噪音）。
- `request_logs.model_raw` 改为可空；probe 行没有模型概念。
- 自动触发的 probe（启动时、cookie 轮换、token 刷新）保持只写 `accounts` / `account_runtime_state`，不再污染请求日志。
- 管理后台 SSE 订阅从 Logs 页组件提升到 `AdminShell`，现在在任意 tab 触发 probe，Logs 页都能即时收到刷新事件。

### 修复

- 修复 `InvalidCookie`、免费账户被拒、OAuth refresh 认证类错误被误分类为 `upstream_error` 的问题，现在统一归入 `auth_rejected`。
- 修复 probe cookie 命中免费账户时日志被记为 `status=ok` 的假成功现象。

## v1.0.8

### 变更

- 请求入口统一做最小采样参数归一化，默认移除 `top_p` 与 `top_k`，降低不同客户端与 Claude 模型之间的参数兼容问题。
- README 补充参数兼容策略说明，明确这是面向小团队共享场景的有意取舍。

### 修复

- 修复部分客户端在 `claude-opus-4-6` 下因采样参数组合触发 `Invalid request data` 的问题；非 thinking 请求现在也会走兼容兜底。
- 补充流式转发链路对上游 SSE `error` 事件和 eventsource 级错误的记录，避免被误判为普通 `client_abort`。

## v1.0.7

### 变更

- 请求日志改为统一的终态落库路径，`messages` 与 `count_tokens` 都会写入 `request_logs`。
- 管理后台日志页新增请求类型筛选，并将后台 SSE 通知调整为最小结构化 payload。
- 服务端版本号同步更新到 `1.0.7`；管理后台显示的版本现在会跟随实际构建版本更新。

### 修复

- 修复流式请求在 `auth_rejected` 之后前端日志长期停更的问题，本质原因是部分请求终态此前没有落库。
- 修复用户主动取消流式请求时不记录日志的问题，补充 `client_abort` 终态。
- 修复未归类错误不写日志的问题，新增 `internal_error` 兜底状态。
- 修复 `count_tokens` 请求未进入日志体系的问题，并避免成功后等待数据库写入期间继续占用账号并发槽。
- 修复已执行 migration 被改写可能导致现有部署启动失败的问题，`internal_error` schema 变更改为前向迁移。

## v1.0.3

### 新增

- 管理后台账号池新增 OAuth 账号接入流程，支持生成 Claude 授权 URL、粘贴 callback URL 或 code 完成入库。
- 账号模型新增 OAuth 凭据、过期时间、刷新时间、错误状态和运行时用量展示。
- 新增纯 OAuth 账号池调度路径，支持不依赖 cookie 的 Bearer API 直连调用。

### 变更

- 收敛发往 Anthropic 的请求头策略，恢复 `anthropic-beta` 客户端透传合并，移除多余硬编码伪装头。
- 保留 `User-Agent` 与最小必需版本头，降低与真实 CLI/SDK 行为冲突的概率。
- browser emulation 继续限定在 cookie / browser 流程；纯 OAuth 运行态保持轻量 API 调用。
- 清理已废弃 stealth 配置项：`cc_sdk_version`、`cc_node_version`、`cc_stainless_os`、`cc_stainless_arch`、`cc_beta_flags`。

### 修复

- 修复管理端 OAuth token 兑换解析失败问题，对齐 Anthropic 实际 token 端点请求格式。
- 修复 OAuth state 回填链路，支持完整 callback URL、`code#state` 和仅 code 输入。
- 修复纯 OAuth 账号运行态未继承代理配置的问题，避免与 cookie 路径出口不一致。
- 修复纯 OAuth 账号认证失败后仍被反复选中的问题；现在会写回 `last_error` 并标记为 `auth_error`。
- 优化账号池添加弹窗 UI，移除冗余字段与重复按钮，压缩长授权 URL 展示。
