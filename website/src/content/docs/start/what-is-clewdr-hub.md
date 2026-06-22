---
title: 这是什么 / 架构
description: clewdr-hub 的项目定位、核心特性、与同类项目的对比，以及整体架构。
sidebar:
  order: 1
---

clewdr-hub 是基于 [clewdr](https://github.com/Xerxes-2/clewdr) 的**多用户 Claude 共享网关**。

> 单二进制 / SQLite / 无额外提示词 / 原生 Anthropic Messages API

它把 Claude Pro/Max 订阅变成团队 API：账号池轮询、并发槽隔离、per-user 限额、费用追踪，开箱即用。

## 核心特性

- **零依赖部署**：单个静态链接二进制，前端编译嵌入，SQLite WAL 自动建库。
- **透明代理**：直接转发 `/v1/messages`，不注入系统提示词；仅为兼容 Anthropic 模型行为做最小[参数归一化](../../reference/request-compat/)。
- **[OpenAI 兼容入口](../../guides/openai-compat/)**：`POST /v1/chat/completions` + `/v1/models?format=` 协商，零改造对接 OpenAI SDK 客户端，复用同一套鉴权 / 配额 / 限流 / 计费链路。
- **标准化伪装**：内置 Claude Code 请求伪装与匹配 UA，减少可配置差异。
- **[多账号调度](../../guides/account-pool/)**：Cookie / OAuth / Custom Anthropic API Key 账号池 + round-robin + 亲和性缓存 + per-account 并发槽（`max_slots`），支持标记账号「优先消耗」。
- **[多代理管理](../../guides/proxies/)**：可维护多个备用代理，支持账号级绑定，适合不同账号走不同出口。
- **[团队隔离](../../guides/teams/)**：用户 → 策略 → API Key，并发 / RPM / 周预算 / 月预算多重限额。
- **管理后台**：总览 / 运维 / 账号池 / 用户 / Key / 日志 / 设置，SSE 实时推送。
- **自适应探测**：自动识别 Pro/Max 账号类型，按实际用量窗口显示。

## 与同类项目对比

|  | **clewdr-hub** | **Sub2API** | **CLIProxyAPI** | **clewdr**（原版） |
|--|---------------|-------------|-----------------|------------------|
| 定位 | 小团队自用网关 | 商业级中转/拼车平台 | 多 provider 代理 | 个人轻代理 |
| 部署 | Rust 单二进制 + SQLite | Go + PostgreSQL + Redis | Go 单二进制 | Rust 单二进制 |
| 支持 provider | Claude 专精 | Claude / OpenAI / Gemini / Antigravity | Gemini / OpenAI / Claude / Codex / Qwen | Claude |
| 代理方式 | cookie → 原生 Messages API | OAuth + cookie | OAuth 包装 CLI | cookie |
| 提示词注入 | **无**，透明转发 | 有平台层注入 | 有 | 无 |
| 用户端 UA 校验 | **不做**，自由接入 | 有 | 有 | 无 |
| 伪装 | 内置标准化 profile | 内置 | 内置 | 可配版本号 |
| 多用户 | 用户/策略/Key/RBAC | 用户/Key/计费/支付 | 管理 API | 单 admin |
| 管理后台 | 内嵌 7 页 React | Vue 全功能后台 | 社区 Dashboard | 配置页 |
| 适合规模 | 3–10 人 | 10–1000+ 人 / 商用 | 个人–中小团队 | 个人 |
| 资源占用 | ~20MB RAM | PG + Redis + Go | ~50MB RAM | ~15MB RAM |

**如果你是 3–10 人团队共享 Claude 订阅，要轻、要透明、不想运维数据库——这个项目就是为你写的。**

## 与原版 clewdr 的关系

fork 自 [clewdr](https://github.com/Xerxes-2/clewdr)，保留其核心代理能力（轻量伪装、cookie 认证、无提示词注入），重构为多用户网关：

- **新增**：用户/策略/RBAC、API Key 认证（blake3）、账号池并发槽调度、请求日志与费用追踪、运维统计与日志下钻、管理后台（7 页）、SSE 实时事件、审计字段、OpenAI 兼容入口。
- **移除**：原版 `/code/v1/*` 路由。OpenAI 兼容能力已通过 `/v1/chat/completions` 完整重做，详见 [OpenAI 兼容入口](../../guides/openai-compat/)。

## 整体架构

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

请求生命周期、调度器、认证、数据库等实现细节见[开发文档](../../dev/architecture/)。
