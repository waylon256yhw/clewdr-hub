---
title: 已知问题与设计决策
description: plaintext_key、默认密码、流式 slot 泄漏边界、inflight 不持久化，以及 OpenAI 兼容入口的演进。
sidebar:
  order: 11
---

## plaintext_key 明文存储

见[认证体系 · 关于 plaintext_key](../auth/#关于-plaintext_key)。这是面向高信任小团队的有意取舍，不是遗漏。

## 默认 admin 密码

未设 `ADMIN_PASSWORD` 时初始密码为 `password`，配合 `must_change_password` 标志强制首次改密。适合本地/内网快速启动，**不要在公网裸露时使用默认密码**。

## 流式 slot 泄漏的边界情况

流式请求通过 `SlotDropGuard`（Drop trait + `tokio::spawn`）在 stream 被 drop 时释放 slot。极端情况下（tokio runtime 已关闭），spawn 可能失败导致 slot 泄漏。实际影响：该账号少一个可用并发，直到下次 reload。对于 3–10 人规模可以接受。

## max_slots 不持久化 inflight

inflight 计数是纯内存状态，进程重启归零。这是正确的——重启意味着所有请求已终止，没有真正的 in-flight。`do_reload()` 保留 inflight 计数是为了处理运行时 admin 操作（增删账号）不丢失正在进行的请求。

## OpenAI 兼容入口的演进

早期 fork 时**移除**了原版 clewdr 的 OAI 兼容层——它需要请求/响应格式转换，增加维护成本和出错面，而当时项目聚焦 Claude Code 场景，直接用 Anthropic Messages API 减少一层抽象。

后来在明确的设计约束下，用 `POST /v1/chat/completions` **完整重做**了 OpenAI 兼容入口：内部翻译成 Anthropic Messages，复用同一套鉴权 / 配额 / 限流 / 计费链路；对未支持字段静默忽略，对无法在 Anthropic 上等价实现的字段（`n > 1`、`logprobs`）硬性拒绝，避免静默偏差。完整契约见[使用指南 · OpenAI 兼容入口](../../guides/openai-compat/)。
