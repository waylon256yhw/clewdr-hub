---
title: 环境变量
description: clewdr-hub 支持的环境变量及其默认值。
sidebar:
  order: 1
---

所有配置都能通过 `CLEWDR_` 前缀的环境变量覆盖 `clewdr.toml` 中的同名项。最常用的几个：

| 变量 | 默认 | 说明 |
|------|------|------|
| `CLEWDR_IP` | `0.0.0.0` | 监听地址 |
| `CLEWDR_PORT` | `8484` | 监听端口 |
| `ADMIN_PASSWORD` | `password` | 管理员密码（首次登录强制修改） |
| `CLEWDR_TRUSTED_PROXIES` | `["127.0.0.0/8", "::1/128", "172.16.0.0/12"]` | 信任的反代来源 CIDR，见[反向代理与真实 IP](../../guides/reverse-proxy/) |
| `CLEWDR_NO_FS` | `false` | 无文件系统模式（内存 SQLite，重启丢数据），仅用于 Hugging Face Space 等场景 |
| `CLEWDR_DEBUG_COOKIE` | `false` | 临时排障：手动 probe 时 dump 原始上游 JSON，见[开发 · 调试配置](../../dev/environment/) |

## 数组类型变量的写法

像 `CLEWDR_TRUSTED_PROXIES` 这种数组值，**必须是 TOML 数组字面量**，整个串放进单引号；逗号分隔的裸字符串**不会**被解析为数组，会导致配置静默回退到默认值。

```bash
# 正确
export CLEWDR_TRUSTED_PROXIES='["127.0.0.0/8", "::1/128", "10.42.0.0/16"]'

# 错误：会被当成单个字符串，解析失败后回退默认
# export CLEWDR_TRUSTED_PROXIES="127.0.0.0/8,::1/128,10.42.0.0/16"
```

Docker Compose 写法：

```yaml
environment:
  CLEWDR_TRUSTED_PROXIES: '["127.0.0.0/8", "::1/128", "10.42.0.0/16"]'
```

更完整的配置（含调试开关）见 [clewdr.toml 配置](../configuration/)。
