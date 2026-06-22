---
title: 管理后台导览
description: clewdr-hub 内嵌管理后台的各个页面及其用途。
sidebar:
  order: 1
---

后台地址即服务根路径（`http://你的IP:8484`），管理员登录后可见。所有页面支持 SSE 实时推送。

## 页面一览

| 页面 | 用途 |
|------|------|
| **总览** | 账号/用户/Key 数量，请求量，当前内置伪装版本 |
| **运维** | 累计请求/Token/金额，模型分布，用户用量趋势与日志下钻 |
| **账号池** | 添加/管理 [Cookie / OAuth / Custom Anthropic API Key 账号](../account-pool/)，给账号绑定代理，查看订阅账号用量窗口和重置倒计时 |
| **代理** | 维护多个[备用代理](../proxies/)，测试基础连通性/延迟/出口 IP/地区 |
| **用户** | [成员 CRUD + 策略管理](../teams/)（并发/RPM/预算） |
| **API Keys** | 创建/绑定/管理 Key，每条 key 可单独开[自动缓存](../prompt-caching/) |
| **日志** | 请求明细，按用户/状态/模型/时间筛选，点击展开详情 |
| **设置** | 模型列表管理、推理 Effort 强制值、改密 |

## 设置项说明

- **模型列表**：控制 `/v1/models` 返回内容，可添加自定义模型 ID。禁用 ≠ 不可调用，只是不列出。
- **推理 Effort 强制值**：可强制覆盖受支持推理模型的 `output_config.effort`，详见[请求参数兼容策略](../../reference/request-compat/)。

`/v1/models` 的格式协商（OpenAI / Anthropic 形态）见 [/v1/models 格式协商](../../reference/models-endpoint/)。
