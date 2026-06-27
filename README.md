<p align="center">
  <img src="assets/logo.svg" width="140" alt="clewdr-hub" />
</p>

# clewdr-hub

基于 [clewdr](https://github.com/Xerxes-2/clewdr) 的多用户 Claude 共享网关。

```
单二进制 / SQLite / 无额外提示词 / 原生 Anthropic Messages API
```

把 Claude Pro/Max 订阅变成团队 API：账号池轮询、并发槽隔离、per-user 限额、费用追踪，开箱即用。

> **📖 完整文档：<https://waylon256yhw.github.io/clewdr-hub/>**
> 安装部署、使用指南、参考与开发文档都在文档站，本 README 只讲快速上手。

---

## 特性

- **零依赖部署**：单个静态链接二进制，前端编译嵌入，SQLite WAL 自动建库
- **透明代理**：直接转发 `/v1/messages`，不注入系统提示词；仅做最小参数归一化
- **OpenAI 兼容入口**：`POST /v1/chat/completions` + `/v1/models?format=` 协商，零改造对接 OpenAI SDK 客户端
- **多账号调度**：Cookie / OAuth / Custom Anthropic API Key 账号池 + round-robin + 亲和性缓存 + per-account 并发槽，支持「优先消耗」
- **多代理管理**：维护多个备用代理，支持账号级绑定，不同账号走不同出口
- **团队隔离**：用户 → 策略 → API Key，并发 / RPM / 周预算 / 月预算多重限额，超限直接拒绝
- **管理后台**：总览 / 运维 / 账号池 / 代理 / 用户 / Key / 日志 / 设置，SSE 实时推送
- **标准化伪装**：内置 Claude Code 请求伪装与匹配 UA，减少可配置差异

## 快速开始

一键安装（Linux / macOS / Termux），自动注册开机自启：

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/install.sh | bash
```

装完打开交互式管理菜单（查看状态、改密码、导入导出配置都在里面）：

```bash
clewdr menu
```

管理后台地址 `http://你的IP:8484`，默认密码 `password`，首次登录强制改密。

> Docker Compose / 宝塔面板 / Hugging Face Space / 手动安装见 [安装部署文档](https://waylon256yhw.github.io/clewdr-hub/start/installation/)。

> **🤖 用 AI agent 部署？** 请先让它读 [AGENTS.md](AGENTS.md)。**生产环境不要从源码 `cargo build`**——优先用上面的一键脚本或 GHCR 预构建 Docker 镜像；从源码构建仅用于二次开发/调试。

## 使用

后台 **账号池** 加账号 → **API Keys** 建 key，然后配置客户端：

```bash
export ANTHROPIC_BASE_URL=http://你的IP:8484
export ANTHROPIC_API_KEY=sk-...    # 从后台创建
```

官方 Claude Code、任意 Anthropic SDK、OpenAI SDK 都能直接接入。多人团队再加[用户与策略](https://waylon256yhw.github.io/clewdr-hub/guides/teams/)。完整流程与第一个请求示例见[第一个请求](https://waylon256yhw.github.io/clewdr-hub/start/first-request/)。

## 与同类项目对比

|  | **clewdr-hub** | **Sub2API** | **CLIProxyAPI** | **clewdr**（原版） |
|--|---------------|-------------|-----------------|------------------|
| 定位 | 小团队自用网关 | 商业级中转/拼车平台 | 多 provider 代理 | 个人轻代理 |
| 部署 | Rust 单二进制 + SQLite | Go + PostgreSQL + Redis | Go 单二进制 | Rust 单二进制 |
| 支持 provider | Claude 专精 | Claude / OpenAI / Gemini | Gemini / OpenAI / Claude / Codex | Claude |
| 提示词注入 | **无**，透明转发 | 有平台层注入 | 有 | 无 |
| 用户端 UA 校验 | **不做**，自由接入 | 有 | 有 | 无 |
| 多用户 | 用户/策略/Key/RBAC | 用户/Key/计费/支付 | 管理 API | 单 admin |
| 适合规模 | 3–10 人 | 10–1000+ 人 / 商用 | 个人–中小团队 | 个人 |
| 资源占用 | ~20MB RAM | PG + Redis + Go | ~50MB RAM | ~15MB RAM |

**如果你是 3–10 人团队共享 Claude 订阅，要轻、要透明、不想运维数据库——这个项目就是为你写的。**

## 关于本项目

fork 自 [clewdr](https://github.com/Xerxes-2/clewdr)，保留其核心代理能力（轻量伪装、cookie 认证、无提示词注入），重构为多用户网关。

- **新增**：用户/策略/RBAC、API Key 认证（blake3）、账号池并发槽调度、请求日志与费用追踪、运维统计与日志下钻、管理后台（7 页）、SSE 实时事件、OpenAI 兼容入口
- **移除**：原版 `/code/v1/*` 路由（OpenAI 兼容已通过 `/v1/chat/completions` 完整重做）

贡献与二次开发见[开发文档](https://waylon256yhw.github.io/clewdr-hub/dev/environment/)。

## 致谢

[clewdr](https://github.com/Xerxes-2/clewdr) by [Xerxes-2](https://github.com/Xerxes-2)

## License

AGPL-3.0
