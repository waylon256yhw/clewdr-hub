---
title: 请求参数兼容策略
description: 服务端对请求参数（temperature / top_p / thinking / effort）的归一化规则。
sidebar:
  order: 3
---

为兼容不同客户端和不同 Claude 模型，服务端会对部分请求参数做归一化。原生 `/v1/messages` 和 [OpenAI 兼容入口](../../guides/openai-compat/)共用这套规则。

## 采样参数

- 服务端会统一移除 `top_p` 和 `top_k`。
- 如果启用原生 `thinking`（`enabled` / `adaptive`），不符合 Anthropic 要求的 `temperature` 也会被移除。

:::note
这是有意的兼容性取舍：对本项目的目标场景，保留 `temperature` 作为主要采样旋钮已经足够，同时减少不同客户端和不同 Claude 模型之间的参数兼容问题。
:::

## thinking 归一化

- `claude-opus-4-7` / `claude-opus-4-8` / `claude-fable-5`：`thinking.type=enabled` + `budget_tokens` 会被重写为 `{type:"adaptive","display":"summarized"}`；如果请求里没带 `output_config.effort`，服务端会显式补成 `high`。这些模型不支持 extended thinking budgets，保持 `enabled` 在上游会被拒绝或忽略，客户拿不到思考链。
- `claude-fable-5` **强制开启思考**：未提供 `thinking` 或显式传入 `disabled` 时，也会规范为 `{type:"adaptive","display":"summarized"}` 并默认补 `effort=high`。

## fable-5 server-side fallback

`claude-fable-5` 默认启用 Anthropic server-side fallback：所有 Messages 请求都会附带 `fallbacks=[{model:"claude-opus-4-8"}]` 和对应 beta 标记。Fable 的安全分类拒绝会在同一请求、同一条流中自动切换到 Opus 4.8；如果 fallback 仍未产生答案，原生入口保持 `stop_reason="refusal"`，OpenAI 兼容入口映射为 `finish_reason="content_filter"`。

## 推理 Effort 强制值

后台「设置」页的「推理 Effort 强制值」可强制覆盖受支持推理模型的 `output_config.effort`（覆盖客户端发送的值）：

- 当前对 `claude-fable-5`、`claude-opus-4-5` / `4-6` / `4-7` / `4-8`（含 8 位日期后缀）写入所选 effort，其他模型完全透传客户端原始值。
- 如果管理员选择了旧版 Opus 不支持的 effort，服务端会自动映射到兼容等级：

  | 模型 | 支持档位 | 映射规则 |
  |------|----------|----------|
  | `fable-5` / `opus-4.7` / `opus-4.8` | `low/medium/high/xhigh/max` 全五档 | 直接写入 |
  | `opus-4.6` | 缺 `xhigh` | `xhigh` → `max` |
  | `opus-4.5` | 缺 `xhigh` / `max` | `xhigh` / `max` → `high` |
