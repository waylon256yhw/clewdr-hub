---
title: 账号池
description: 账号池支持 Cookie / OAuth / Custom Anthropic API Key 三类上游账号，以及「优先消耗」标记。
sidebar:
  order: 2
---

账号池是 clewdr-hub 的上游来源。在后台 **账号池** 页添加、管理账号，并可给账号绑定[代理](../proxies/)。

## 三类上游账号

| 类型 | 面向 | 说明 |
|------|------|------|
| **Cookie / OAuth** | Claude Pro / Max 订阅账号 | 后台会探测账号类型、用量窗口和重置时间 |
| **API Key** | 官方 Anthropic API key 或兼容端点 | 例如需要 `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` 和额外请求头的内部 / 云厂商代理 |

## 添加 Custom Anthropic API Key 账号

填写以下字段：

- **基础 URL**：例如 `https://api.anthropic.com/` 或自定义 Anthropic-compatible endpoint；服务端会规范化后拼接 `/v1/messages`、`/v1/messages/count_tokens`。
- **API 密钥**：通过 `x-api-key` 发送；编辑账号时留空表示保留原值。
- **额外请求头**：用于工作区、租户或自定义路由信息，例如 `anthropic-workspace-id`。这些值会在管理员账号编辑页回显，方便维护；导出 `--no-secrets` 和运行日志仍会避免泄露 API key 与 header 值。
- **中转伪装 (Mimicry)**：可选。指向**中转站**且对方会前置校验请求形态时，开启 [Claude Cloak](../claude-cloak/) 把请求塑造成 Claude Code CLI；直连官方 / 云厂商端点保持**关闭**。

:::note
API Key 账号不会显示 5h / 7d 订阅用量窗口，也不会参与全量订阅探测。请求日志、用户配额、计费统计、OpenAI 兼容入口仍走同一套链路。
:::

## 优先消耗（drain_first）

账号级开关，把临时额度、试用额度或希望先用掉的账号放在调度前面。

- **适合**：限量试用 / 促销账号，先榨干再动主力账号。
- **不要**：给所有账号都打开（等同于没有优先级），也不要把高价值主力账号误标为优先消耗。

调度器如何处理优先消耗、亲和性缓存与并发槽的交互，见[账号调度器](../../dev/scheduler/)。

## 用量窗口与探测

Cookie / OAuth 订阅账号会被自动探测：识别 Pro / Max 类型、用量窗口和重置倒计时，在账号池页展示。手动触发 cookie probe 的调试方法见[数据库 · Cookie Probe 调试](../../dev/database/)。
