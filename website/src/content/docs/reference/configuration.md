---
title: clewdr.toml 配置
description: clewdr.toml 的进程级配置项。账号、用户、Key、代理、模型等运行时数据都在 SQLite，通过管理后台维护。
sidebar:
  order: 2
---

clewdr-hub 的配置分两层：

- **进程级配置**：`clewdr.toml`（监听地址、更新策略、反代信任等）。
- **运行时数据**：账号、用户、API Key、代理、模型列表、伪装版本等都存在 SQLite，通过[管理后台](../../guides/admin-console/)维护，**不写在 toml 里**。

## 文件位置

- systemd 部署：`/opt/clewdr/clewdr.toml`，改完执行 `systemctl restart clewdr`。
- 二进制 / 开发：进程工作目录下的 `clewdr.toml`。

所有项都能用 `CLEWDR_` 前缀的[环境变量](../environment-variables/)覆盖（例如 `CLEWDR_PORT`）。

## 配置项

```toml
ip = "0.0.0.0"
port = 8484
check_update = true
auto_update = false
no_fs = false
log_to_file = false
debug_cookie = false
trusted_proxies = ["127.0.0.0/8", "::1/128", "172.16.0.0/12"]
```

| 键 | 默认 | 说明 |
|----|------|------|
| `ip` | `"0.0.0.0"` | 监听地址 |
| `port` | `8484` | 监听端口 |
| `check_update` | `true` | 启动时检查新版本 |
| `auto_update` | `false` | 是否自动更新 |
| `no_fs` | `false` | 无文件系统模式（内存 SQLite，关闭一切文件写入）。仅用于 HF Space 等场景，日常勿开 |
| `log_to_file` | `false` | 是否写文件日志 |
| `debug_cookie` | `false` | 手动 probe 时 dump 原始上游 JSON 到 `log/probe-dumps/`，仅临时排障用 |
| `trusted_proxies` | 见上 | 信任的反代来源 CIDR，详见[反向代理与真实 IP](../../guides/reverse-proxy/) |

`debug_cookie` / `no_fs` 的语义和排障用法见[开发 · 调试配置](../../dev/environment/)。
