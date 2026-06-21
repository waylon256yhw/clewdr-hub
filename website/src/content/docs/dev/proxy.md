---
title: 代理与测试
description: 代理资源模型、账号级绑定的生效路径，以及服务器侧代理测试的实现与日志。
sidebar:
  order: 7
---

用户侧的代理操作见[代理管理](../../guides/proxies/)。本页讲实现。

## 代理模型

- 代理已经从旧的全局设置项提升为独立资源，存放在 `proxies` 表。
- 一个实例可同时维护多个备用代理；当前只做「保存多个、按需选择」，不做自动轮换。
- 每个账号可通过 `accounts.proxy_id` 绑定一个代理，也可以留空直连。

## 生效路径

账号级代理会影响这些出站路径：

- Claude Messages 请求
- OAuth callback 交换
- OAuth refresh
- OAuth probe / account test

实现上通过 `load_all_accounts()` 组装 `proxy_url`，再传给 `ClaudeCodeState` / `oauth.rs` 统一构建带代理的 `wreq::Client`。

## 代理测试

管理后台「代理」页的测试是服务器侧的通用连通性测试，不是某个特定上游服务的兼容性测试。当前行为：

- 通过代理请求 `https://ipwho.is/`，失败时回退 `https://httpbin.org/ip`
- 记录延迟、出口 IP、地区信息
- 地区补全通过 `IP2Location.io` 在线查询完成
- 成功/失败结果会持久化到 `proxies.last_test_*`

## 代理测试日志

- 每次代理测试都会写一条 `request_logs`，类型为 `probe_proxy`
- `response_body` 保存结构化 JSON bundle，包含：
  - 代理基础信息：仅 `id / name / protocol / host / port`
  - 上游探测尝试列表
  - 地区补全返回
  - 最终测试结果

:::note
日志 bundle 明确**不记录**代理用户名、密码，也不记录带凭据的完整代理 URL。
:::
