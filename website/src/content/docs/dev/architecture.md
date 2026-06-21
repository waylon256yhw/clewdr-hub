---
title: 架构与请求生命周期
description: clewdr-hub 的整体架构，以及 POST /v1/messages 一次请求从认证到上游的完整生命周期。
sidebar:
  order: 3
---

## 架构概览

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

## 请求生命周期

以 `POST /v1/messages` 为例：

1. **认证**（`RequireFlexibleAuth`）：从 `x-api-key` 或 `Authorization: Bearer` 提取 key → 查 `api_keys` 表 → blake3 校验 → 注入 `AuthenticatedUser` 到 request extensions。同时 fire-and-forget 更新 `last_used_at` / `last_used_ip` / `last_seen_at`。

2. **预处理**（`ClaudeCodePreprocess`）：解析请求体 → 提取模型名 → 构建 `ClaudeContext`（request_id, user_id, api_key_id, bound_account_ids, started_at）。如果当前 API key 打开了 `auto_cache_enabled`，并且不是 `/v1/messages/count_tokens` 路径，会在 body 顶层注入 `cache_control: {type: "ephemeral"}`（`apply_auto_cache` helper，[OpenAI 兼容入口](../../guides/openai-compat/)同样接入）。

3. **限流**（handler 内）：
   - `UserLimiterMap::try_acquire()`：per-user semaphore，基于策略的 `max_concurrent`
   - RPM 检查：滑动窗口
   - 预算检查：查 `usage_rollups` 表的周/月累计

4. **调度**（`AccountPoolActor::dispatch()`）：
   - `bound_account_ids` 非空时只从绑定账号中选
   - 检查 `inflight < max_slots`
   - 亲和 key 优先来自官方 Claude Code 透传的 `metadata.user_id` 中的 `_session_`，缺失时回退到 cache-control system blocks 的哈希
   - **Phase A**：moka 亲和性缓存命中且账号仍可用 → 返回同一 `account_id`
   - 缓存账号仅因 `inflight >= max_slots` 满槽时，临时借用其他可用账号（优先 `drain_first`），但不改写缓存
   - **Phase B**：无有效缓存时优先选择 `drain_first = true` 且满足绑定约束、仍有空闲槽的账号
   - **Phase C**：否则 round-robin 取第一个可用账号
   - `inflight += 1`

   调度算法的完整规则见[账号调度器](../scheduler/)。

5. **Token 交换**：检查账号的 access_token 状态（None/Expired/Valid），必要时走 cookie → OAuth code → token 或纯 OAuth refresh 流程。

6. **代理绑定**：如果账号配置了 `proxy_id`，会先解析成账号级 `proxy_url`，后续 Claude 请求、OAuth refresh、OAuth probe 都走这个代理；未绑定则直连。

7. **上游请求**：构建伪装请求头（stealth profile）→ `POST api.anthropic.com/v1/messages`。

8. **响应处理**：
   - 非流式：读取完整响应 → 提取 usage → 持久化计费 → 返回
   - 流式：返回 SSE stream → 在 `MessageStop` 事件中异步持久化 usage + 释放 slot
   - 流异常终止（客户端断开）：`SlotDropGuard` 的 `Drop` impl 自动释放 slot

9. **重试**：遇到 429/auth 错误时 `release(Some(reason))` 将账号移入 exhausted 队列 → 换一个账号重试，最多 6 次。
