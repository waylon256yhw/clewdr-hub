---
title: /v1/models 格式协商
description: /v1/models 通过 ?format= 查询字符串在 OpenAI 与 Anthropic 形态之间协商，外加 1M Context 说明。
sidebar:
  order: 4
---

`/v1/models` 通过 `?format=` 查询字符串决定返回形态，方便不同客户端无缝对接。

| 查询字符串 | 形态 | 用途 |
| -- | -- | -- |
| 缺省 / `?format=` | 兼容超集（同时含 Anthropic `display_name/created_at/type/has_more/first_id/last_id` 和 OpenAI `object/created/owned_by`） | 老客户端无感升级，默认行为 |
| `?format=openai` | 严格 OpenAI：顶层 `object="list"`，`data[].object="model"`，`created` 是 Unix 秒，`owned_by="anthropic"` | OpenAI SDK / LiteLLM / one-api 风中转 |
| `?format=anthropic` | 当前 Anthropic 形态（无 `object` / `created` / `owned_by`） | 严格按 Anthropic models API 解析的客户端 |
| 其他值 | 400 `invalid_request_error` | 阻止拼写错误静默走默认 |

同一个查询字符串也作用于 `GET /v1/models/{id}` 单条接口。

返回哪些模型由后台「设置 → 模型列表」控制，见[管理后台导览](../../guides/admin-console/)。

## Anthropic 1M Context 说明

- 本项目不再支持 legacy `-1M` 伪模型名，请直接使用 Anthropic 官方标准模型名。
- 本项目不会主动添加 `context-1m-2025-08-07`，也会忽略客户端传入的这个 legacy beta header。
- 依赖 `context-1m-2025-08-07` 的过渡 1M beta 已在 `2026-04-30` 后退出支持范围；需要 1M context 时请使用已经原生支持该能力的官方模型名。
