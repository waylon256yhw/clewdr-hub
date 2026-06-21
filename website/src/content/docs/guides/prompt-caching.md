---
title: Prompt Caching
description: per-key 的 Automatic Prompt Caching 开关——何时开、何时不要开。
sidebar:
  order: 5
---

API Keys 页面每条 key 都有一个**自动缓存**开关。

## 行为

- **开启时**：服务端会在每个出口请求体顶层注入 `"cache_control": {"type": "ephemeral"}`。Anthropic 服务端会自动把缓存断点放在最后一个可缓存 block 上，并随多轮对话自动前移。命中后只对 prompt 收 0.1x 的 cache-read 价。
- **关闭时**：不注入任何字段，请求按客户端原样透传，行为与之前一致。
- **`/v1/messages/count_tokens` 路径不会注入**（缓存对它无意义）。

## 生效阈值

达不到阈值只是没收益、不会报错：

| 模型 | 最小可缓存 token |
|------|------------------|
| Sonnet | ≥ 1024 |
| Opus / Haiku 4.5 | ≥ 4096 |

## 何时开 / 何时不要开

:::tip[适用场景]
上下文稳定增长的多轮对话——聊天、Agent 反复读同一份资料。
:::

:::caution[不要开的场景]
- 客户端已经自己管理 cache 断点（比如官方 Claude Code，自己会塞 3~4 个，再加一个可能超出 4 个 slot 上限触发 400）。
- 经常重写历史 / 插入中间消息的工作流——每改一次都会让 prefix hash 变化命中失败。
:::

第一版 TTL 锁死默认 5 分钟（每次命中重置倒计时），暂不暴露 1h 选项。
