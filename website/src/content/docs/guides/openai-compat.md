---
title: OpenAI 兼容入口
description: POST /v1/chat/completions 接受标准 OpenAI Chat Completions 请求体，内部翻译成 Anthropic Messages 调用。
sidebar:
  order: 6
---

`POST /v1/chat/completions` 接受标准 OpenAI Chat Completions 请求体，内部翻译成 Anthropic Messages 调用同一套上游链路。鉴权、配额、限流、计费、日志都和 `/v1/messages` 共享——所以 `RequestType` 仍记为 `messages`，`/api/admin/logs` 不需要新过滤项。

`/v1/models` 的格式协商见 [/v1/models 格式协商](../../reference/models-endpoint/)。

## 最小客户端示例

```python
from openai import OpenAI

client = OpenAI(base_url="http://你的IP:8484/v1", api_key="sk-...")
resp = client.chat.completions.create(
    model="claude-sonnet-4-6",
    messages=[{"role": "user", "content": "Hi"}],
)
print(resp.choices[0].message.content)
```

## 支持的字段

`messages`（含 `developer` 角色，自动并入 `system`）、`stream` + `stream_options.include_usage`、`tools` / `tool_choice`（含 `required` → Anthropic `any`，`none` 自动剥除工具）、`response_format`（`text` / `json_object` 静默接受，`json_schema` 映射到 `output_format`）、`reasoning_effort`（`low/medium/high`）、`max_tokens` / `max_completion_tokens`、`stop` 字符串或数组（自动截到 4 条）、`temperature`、`user`、字符串型 `metadata`。

## 行为细节

- **多模态**：`image_url` 内容部分支持 `data:image/{jpeg|png|gif|webp};base64,...` 数据 URL 和 `http(s)://` URL；其它 MIME / scheme 直接 400。
- **思考链**：上游 `thinking` 内容块默认映射到 OpenAI/DeepSeek 约定的 `message.reasoning_content`；客户端发送 `x-include-reasoning: false`（或 `0` / `no`）可关闭。
- **流式**：响应是 `text/event-stream`，每个 chunk 是 `chat.completion.chunk`。开启 `stream_options.include_usage` 后，`[DONE]` 之前会单独发一个 `choices=[], usage={...}` chunk（含 `prompt_tokens_details.cached_tokens`）。
- **Usage 归并**：`prompt_tokens = input + cache_creation + cache_read`，`cache_read` 单独经 `prompt_tokens_details.cached_tokens` 透出。

## 兼容性边界

- **静默忽略**：`frequency_penalty` / `presence_penalty` / `logit_bias` / `seed` / `service_tier` / `store` / `top_logprobs`、非 string 的 `metadata` 字段、未识别字段；接收但不映射到上游。
- **硬性拒绝**：`n > 1` 与 `logprobs: true` 显式 400——语义无法在 Anthropic 上等价实现，避免静默偏差。
- **错误体**：所有错误（鉴权失败、配额超限、上游池空、上游 4xx/5xx）都按 `{ "error": { "message", "type", "code", "param" } }` 返回；状态码与 `/v1/messages` 同一信号保持一致。
- **CORS**：preflight 接受 `openai-beta` / `openai-organization` / `openai-project` 三个浏览器侧客户端常用 header。

请求参数（如 `temperature` / `top_p` / `thinking`）在两个入口共用的归一化规则见[请求参数兼容策略](../../reference/request-compat/)。
