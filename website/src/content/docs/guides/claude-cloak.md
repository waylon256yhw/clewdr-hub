---
title: Claude Cloak（第三方中转伪装）
description: 给 API Key 渠道可选开启的 Claude Code 请求伪装，用于通过中转站（中转 API）的前置请求校验。
sidebar:
  order: 8
---

许多**中转站（第三方中转 API）**为保护号池、规范用户行为，会在**收到请求时前置校验请求头 / 请求体**——不像 Anthropic 官方那样事后检测，而是当场按硬性规则拒绝不像 Claude Code CLI 的请求（常见报错：`Client error, please upgrade your Claude Code client`）。

**Claude Cloak** 是账号池里 **API Key 渠道**的一个可选开关：开启后，本渠道的出站请求会被塑造成真实 Claude Code CLI 的形态，从而通过这类校验。名称致敬移植自的独立项目 Claude-Cloak。

:::note
这是**第三方中转专用**的伪装，与官方订阅路径（Cookie / OAuth）的伪装是两套东西。官方路径始终、强制套用一套稳定的 Claude Code 指纹（对手是 Anthropic 风控），且**不可配置**；Claude Cloak 面向的是中转商的前置校验，允许自定义、按需开启。
:::

## 什么时候开 / 不开

| 上游类型 | 是否开启 Claude Cloak |
|----------|----------------------|
| 中转站 / 第三方中转 API（前置校验 CLI 形态） | **开** |
| 官方 `api.anthropic.com` 按量付费 key | 不开（正规通道，套 CLI 头反而多余、且可能触发严格企业代理的拦截） |
| AWS Bedrock / 云厂商 Anthropic-compatible 端点 | 不开 |

一句话：**只有当上游会因为"请求不像 Claude Code"而拒绝时才开。**

## 开启方式

后台 **账号池** → 新增或编辑一个 **API Key** 账号 → **中转伪装 (Mimicry)** 区块：

- **关闭**：清洁直连（默认，历史行为不变）——只发 `x-api-key` + `anthropic-version` + 你配置的额外请求头。
- **Claude Cloak**：套用完整的 Claude Code 伪装（见下）。

选择 **Claude Cloak** 后展开以下逐渠道选项：

| 选项 | 说明 | 默认 |
|------|------|------|
| **认证头** | 密钥以 `Authorization: Bearer` 还是 `x-api-key` 发送。多数中转站要 Bearer | `Authorization: Bearer` |
| **渠道覆盖 CLI 版本** | 本渠道模拟的 CLI 版本；留空继承全局默认 | 留空（继承全局） |
| **严格 system 模式** | 把客户端自带的 `system` 下沉为首条 `user` 消息，wire 上只保留 Claude Code 身份 | 开 |
| **额外 anthropic-beta** | 逗号或换行分隔，用于中转站要求的额外 beta token | 空 |

其它固定项（billing 头、stainless 头、参数规范化、`cch` 占位、身份注入等）已固化在伪装 profile 中，**不作为逐项开关**，以免配出"半套指纹"。

## 全局默认 CLI 版本

**设置**页有 **第三方中转伪装默认 CLI 版本**，作为所有开启 Claude Cloak 的渠道的默认模拟版本。

- 优先级：**渠道覆盖 > 全局默认 > 内置默认**。
- **留空**：使用内置默认版本。
- 下拉会从 npm 拉取 `@anthropic-ai/claude-code` 最近几个版本供选择（1 小时缓存，可手动刷新）；也可**手动输入**任意 `x.y.z`，把版本**固定到某个旧版本**（部分中转站要求）。

:::caution
官方订阅路径的模拟版本是编译期固定的，**不受此设置影响**。这里改的只有第三方中转伪装。
:::

## 开启后到底发了什么

**请求头**（在清洁直连的基础上追加/替换）：

- `Authorization: Bearer <key>`（或按选项用 `x-api-key`）
- `User-Agent: claude-cli/<版本> (external, cli)`、`x-app: cli`、`anthropic-dangerous-direct-browser-access: true`
- `anthropic-version: 2023-06-01`、`Accept: application/json`
- **动态 `anthropic-beta`**：按请求实际用到的能力合成（非 Haiku 加 `claude-code-20250219`、始终加 `interleaved-thinking-2025-05-14`，按 body 特征加 `context-management` / `effort` / `structured-outputs`），并追加渠道的额外 beta。**不带** `oauth-2025-04-20`（那是官方 OAuth 专用），也不继承入站 `anthropic-beta`。
- `x-claude-code-session-id`、整套 `x-stainless-*`；流式请求追加 `x-stainless-helper-method: stream`
- TLS 采用 Node/OpenSSL 指纹（JA4），与上面的 `claude-cli` / `x-stainless-runtime=node` 头一致

**请求体**：

- 注入 Claude Code 身份 system 块（`You are Claude Code, ...`）；严格模式下把客户端 system 下沉为首条 user 消息
- 注入 billing 头 system 块（`x-anthropic-billing-header: cc_version=<版本>.<hash>; cc_entrypoint=cli; cch=00000;`）——`cch` 保留字面 `00000`（中转站通常重算或白名单该值）
- 补全 `metadata.user_id`：保留合法的入站值，否则生成结构合法的随机值
- 通用参数规范化（与官方路径共享同一套）

**count_tokens**：只发上述**请求头**，**不做请求体伪装**（真实 CLI 的 count 请求也不带 billing / metadata / session）。

## 测试

账号卡片上的 **测试** 会用**与真实请求完全相同**的构造发探针请求——对开启 Claude Cloak 的渠道，测试同样套用完整伪装（含 TLS 指纹），所以测试结果与实际流量一致。

## 与"关闭"模式的区别

| | 关闭（清洁直连） | Claude Cloak |
|---|---|---|
| 认证 | `x-api-key` | `Authorization: Bearer`（默认，可切 `x-api-key`） |
| CLI 伪装头 | 无 | UA / x-app / stainless / session / TLS 指纹 |
| billing / 身份 system 块 | 剥除 | 注入 |
| `anthropic-beta` | 透传（去掉 oauth-only token） | 按能力合成 |
| 适用 | 官方 / 云厂商正规端点 | 中转站前置校验 |

## 相关

- [账号池](../account-pool/) —— 添加 API Key 渠道、额外请求头、优先消耗
- [代理管理](../proxies/) —— 给渠道绑定出站代理
