# Release Notes

## clewdr-hub

基于 clewdr 重构的多用户 Claude 共享网关首发版本。

### 新增

- 多用户体系：用户 / 策略 / API Key / RBAC
- 账号池调度：round-robin + 亲和性缓存 + per-account 并发槽（max_slots）
- 请求日志与费用追踪（nanousd 精度）、周/月预算控制
- 管理后台 6 页：总览、账号池、用户、Key、日志、设置
- SSE 实时事件推送
- 审计字段：last_used_at / last_used_ip / last_seen_at
- 自适应用量窗口探测（Pro/Max 自动识别）
- CLI 版本伪装（从 npm 拉取最新版本）
- docker-compose.yml 生产部署配置
- pre-commit hook（cargo fmt）

### 变更

- 移除 OpenAI 兼容端点（/v1/chat/completions）
- 移除 /code/v1/* 路由
- Dockerfile 改用 npm ci，DB 持久化到 volume
- CI 前端构建统一为 npm
