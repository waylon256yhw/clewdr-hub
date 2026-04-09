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
- **轻量伪装**：可配置 CLI/SDK 版本号和请求头，过上游客户端检测
- **多账号调度**：cookie 池 + round-robin + 亲和性缓存 + per-account 并发槽（`max_slots`）
- **团队隔离**：用户 → 策略 → API Key，并发/RPM/周预算/月预算多重限额
- **Per-Key 绑定**：把特定 key 锁定到指定账号，隔离资源
- **管理后台**：总览 / 账号池 / 用户 / Key / 日志 / 设置，SSE 实时推送
- **自适应探测**：自动识别 Pro/Max 账号类型，按实际用量窗口显示

## 部署

### Docker Compose（推荐）

```bash
mkdir clewdr-hub && cd clewdr-hub
curl -O https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/docker-compose.yml
docker compose up -d
```

管理后台：`http://your-ip:8484`，默认密码 `password`（首次登录强制改密）。

数据持久化在 Docker volume `clewdr-data` 中，`docker compose down` 不会丢数据。

### 二进制

```bash
# 下载（Linux x86_64 示例，其他架构见 Releases）
curl -fL https://github.com/waylon256yhw/clewdr-hub/releases/latest/download/clewdr-linux-x86_64.zip -o clewdr.zip
unzip clewdr.zip && chmod +x clewdr
./clewdr
```

DB 自动创建在同目录（`clewdr.db`），可用 `--db /path/to/db` 指定。

#### systemd 持久化（推荐）

```bash
# 安装二进制
sudo mkdir -p /opt/clewdr/log
sudo cp clewdr /opt/clewdr/
sudo useradd -r -s /sbin/nologin clewdr 2>/dev/null || true
sudo chown -R clewdr:clewdr /opt/clewdr

# 安装 service + 日志轮转
sudo curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/deploy/clewdr.service \
  -o /etc/systemd/system/clewdr.service
sudo curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/deploy/clewdr.logrotate \
  -o /etc/logrotate.d/clewdr
sudo systemctl daemon-reload
sudo systemctl enable --now clewdr
```

查看状态：`systemctl status clewdr`，日志：`journalctl -u clewdr -f` 或 `tail -f /opt/clewdr/log/*.log`。

### 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `CLEWDR_IP` | `0.0.0.0` | 监听地址 |
| `CLEWDR_PORT` | `8484` | 监听端口 |
| `ADMIN_PASSWORD` | `password` | 管理员密码（首次登录强制修改） |

## 使用

```bash
export ANTHROPIC_BASE_URL=http://your-ip:8484
export ANTHROPIC_API_KEY=sk-...    # 从后台创建
```

流程：**后台登录 → 账号池添加 Cookie → 创建 API Key → 客户端配置上面两行**。单人到这里就够了。

### 请求参数兼容策略

- 服务端会统一移除 `top_p` 和 `top_k`
- 如果启用原生 `thinking`（`enabled` / `adaptive`），不符合 Anthropic 要求的 `temperature` 也会被移除

这是有意的兼容性取舍：对这个项目的目标场景，保留 `temperature` 作为主要采样旋钮已经足够，同时可以减少不同客户端和不同 Claude 模型之间的参数兼容问题。

### Anthropic 1M Context 说明

- 本项目不再支持 legacy `-1M` 伪模型名，请直接使用 Anthropic 官方标准模型名
- 本项目不会主动添加 `context-1m-2025-08-07`，也会忽略客户端传入的这个 legacy beta header
- 截至 `2026-04-09`，`claude-opus-4-6` 与 `claude-sonnet-4-6` 的 1M context 已是原生能力；`claude-opus-4-5` 仍为 `200k`
- Anthropic 已宣布 `claude-sonnet-4` / `claude-sonnet-4-5` 依赖 `context-1m-2025-08-07` 的过渡 1M beta 将在 `2026-04-30` 退场；本项目不再为这条历史兼容路径做适配

### 团队扩展

在上面基础上：

1. **策略**（用户页 → 策略标签）：定义并发/RPM/周月预算模板
2. **用户**：为成员创建账号，分配策略（管理员账号内置，不可新建）
3. **分发 Key**：每人一个 key，可选绑定到特定账号
4. 超限请求直接拒绝，不消耗账号资源

## 后台功能

地址即服务根路径，管理员登录后可见：

| 页面 | 用途 |
|------|------|
| **总览** | 账号/用户/Key 数量，请求量，当前伪装版本 |
| **账号池** | 添加/管理 Cookie，查看用量窗口和重置倒计时 |
| **用户** | 成员 CRUD + 策略管理（并发/RPM/预算） |
| **API Keys** | 创建/绑定/管理 Key |
| **日志** | 请求明细，按用户/状态/模型/时间筛选，点击展开详情 |
| **设置** | CLI 版本伪装、模型列表管理、出站代理、改密 |

### 设置项说明

- **CLI 版本伪装**：从 npm 拉取最新版本号，切换后立即生效。上游更新检测策略时用。
- **模型列表**：控制 `/v1/models` 返回内容，可添加自定义模型 ID。禁用 ≠ 不可调用，只是不列出。
- **出站代理**：`socks5://` 或 `http://` 格式，服务器不能直连时用。

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
| 管理后台 | 内嵌 6 页 React | Vue 全功能后台 | 社区 Dashboard | 配置页 |
| 适合规模 | 3–10 人 | 10–1000+ 人 / 商用 | 个人–中小团队 | 个人 |
| 资源占用 | ~20MB RAM | PG + Redis + Go | ~50MB RAM | ~15MB RAM |

**如果你是 3–10 人团队共享 Claude 订阅，要轻、要透明、不想运维数据库——这个项目就是为你写的。**

fork 自 [clewdr](https://github.com/Xerxes-2/clewdr)，保留其核心代理能力（轻量伪装、cookie 认证、无提示词注入），重构为多用户网关：

**新增**：用户/策略/RBAC、API Key 认证（blake3）、账号池并发槽调度、请求日志与费用追踪、管理后台（6 页）、SSE 实时事件、审计字段

**移除**：OpenAI 兼容端点（`/v1/chat/completions`）、`/code/v1/*` 路由。需要 OpenAI 格式请用原版。

## 致谢

[clewdr](https://github.com/Xerxes-2/clewdr) by [Xerxes-2](https://github.com/Xerxes-2)

## License

AGPL-3.0
