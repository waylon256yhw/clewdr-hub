---
title: 数据库
description: SQLite WAL 模式、核心表清单、费用精度、migration 规范、request_logs 约束，以及 Cookie Probe 调试。
sidebar:
  order: 6
---

SQLite WAL 模式，通过 sqlx 的编译期 migration 自动建表。

## 核心表

| 表 | 用途 | 关键字段 |
|----|------|----------|
| `users` | 用户 | username, role, password_hash (argon2), policy_id, last_seen_at |
| `policies` | 策略模板 | max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd |
| `api_keys` | API Key | user_id, lookup_key, key_hash (blake3), last_used_at, last_used_ip |
| `api_key_account_bindings` | Key↔账号绑定 | api_key_id, account_id |
| `accounts` | Claude 账号 | cookie_blob, proxy_id, max_slots, drain_first, status, email, account_type |
| `proxies` | 代理资源 | name, protocol, host, port, username, password, last_test_* |
| `account_runtime_state` | 运行时状态 | reset_time, 4×用量窗口, 5×usage bucket, 4×utilization |
| `request_logs` | 请求日志 | 全字段（token/cost/ttft/duration/error/response_body），保留 7 天 |
| `usage_rollups` | 费用汇总 | user_id + period_type(week/month) + period_start → cost_nanousd |
| `usage_lifetime_totals` | 累计汇总 | user_id 维度累计 request/token/cost，独立于日志留存 |
| `models` | 模型列表 | model_id, source(builtin/admin/discovered), enabled |
| `settings` | KV 配置 | key → value（session_secret、模型和运行时管理项等） |

## 费用精度

所有金额使用 `nanousd`（1 USD = 10⁹ nanousd）存储，避免浮点精度问题。前端显示时转换为 USD。

## Migration

位于 `migrations/` 目录，sqlx 启动时自动执行。命名规范：`{YYYYMMDD}{seq}_description.sql`。

## request_logs 类型

当前 `request_logs.request_type` 受 SQLite `CHECK` 约束限制，允许值包括：

- `messages`
- `probe_cookie`
- `probe_oauth`
- `probe_proxy`
- `test`

:::caution
新增 probe 类型时，除了改 Rust 枚举 `RequestType`，还**必须**同步补 migration 更新 `request_logs` 的约束。
:::

## Cookie Probe 调试

手动触发 `probe_cookie` 时，正式日志默认只保存精简后的 `bootstrap_summary` 和 `usage`，避免把 `growthbook` / `system_prompts` 一类超大 console bootstrap 原文写进 `request_logs`。

要抓原始上游 JSON：

1. 在 `clewdr.toml` 里把 `debug_cookie = true`（也可用环境变量 `CLEWDR_DEBUG_COOKIE=true ./dev.sh`）
2. 重启后端
3. 从后台手动触发目标账号的 cookie probe
4. 到日志详情查看 `debug_dump_file`
5. 打开 `log/probe-dumps/*.json`

补充说明：

- 只有手动 probe 会带 `debug_dump_file`，自动后台探测默认不写 dump
- 如果 probe 日志本身超过 `PROBE_BODY_MAX_BYTES`，日志行会显示 `truncated=true`，但仍会保留 `debug_dump_file`
- `no_fs = true` 时不会写 dump 文件，因此也不会生成 `debug_dump_file`
