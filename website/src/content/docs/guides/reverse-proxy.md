---
title: 反向代理与真实 IP
description: 让 clewdr-hub 透过 nginx / Caddy 拿到真实客户端 IP——trusted_proxies 配置与各场景示例。
sidebar:
  order: 7
---

clewdr-hub 会把每次请求的「解析后客户端 IP」记录到 `api_keys.last_used_ip`（以及未来的请求审计字段）。**默认 nginx / Caddy 不会自动透传客户端 IP**，需要两边都配：反代设置 `X-Forwarded-For`，clewdr-hub 信任反代来源。

`trusted_proxies` 默认值为 `["127.0.0.0/8", "::1/128", "172.16.0.0/12"]`，覆盖同机部署和 Docker Compose 桥接默认网段。其他场景需要在 `clewdr.toml` 显式追加你反代的来源 CIDR。

## 场景 1：同机 nginx，clewdr 监听 127.0.0.1

nginx 站点配置：

```nginx
server {
    listen 443 ssl;
    server_name api.example.com;

    location / {
        proxy_pass http://127.0.0.1:8484;
        proxy_http_version 1.1;

        # 让 clewdr-hub 看到真实客户端 IP
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # 流式响应
        proxy_buffering    off;
        proxy_read_timeout 600s;
    }
}
```

clewdr-hub 侧无需任何配置改动（loopback 默认在 `trusted_proxies` 内）。

## 场景 2：Docker Compose（nginx + clewdr 同网）

```yaml
services:
  clewdr-hub:
    image: clewdr-hub:latest
    expose:
      - "8484"            # 注意：用 expose 而不是 ports，避免绕过反代直连
    networks: [web]
    # Docker 桥接默认网段 172.16.0.0/12 已在 trusted_proxies 默认里，
    # 不需要额外配置。非默认网段见下面"自定义 trusted_proxies"。

  nginx:
    image: nginx:alpine
    ports: ["443:443"]
    volumes:
      - ./nginx.conf:/etc/nginx/conf.d/default.conf:ro
    networks: [web]

networks:
  web:
```

`nginx.conf`：

```nginx
upstream clewdr { server clewdr-hub:8484; }

server {
    listen 443 ssl;
    server_name api.example.com;

    location / {
        proxy_pass http://clewdr;
        proxy_http_version 1.1;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_buffering    off;
        proxy_read_timeout 600s;
    }
}
```

## 场景 3：Caddy

```text
api.example.com {
    reverse_proxy 127.0.0.1:8484
}
```

Caddy 的 `reverse_proxy` 会自动注入 `X-Forwarded-For` / `X-Forwarded-Proto` / `X-Forwarded-Host`，clewdr-hub 侧零配置。

## 自定义 `trusted_proxies`

如果你的反代在其他来源（Kubernetes Pod CIDR、外部 LB、自定义 Docker 网络），需要显式追加。

**通过 `clewdr.toml`（推荐）**：

```toml
trusted_proxies = ["127.0.0.0/8", "::1/128", "10.42.0.0/16", "192.168.1.0/24"]
```

**通过环境变量**：值必须是 **TOML 数组字面量**，整个串放进单引号；逗号分隔的裸字符串**不会**被解析为数组，会导致配置静默回退到默认值。

```bash
# 正确
export CLEWDR_TRUSTED_PROXIES='["127.0.0.0/8", "::1/128", "10.42.0.0/16"]'

# 错误：会被当成单个字符串，解析失败后回退默认
# export CLEWDR_TRUSTED_PROXIES="127.0.0.0/8,::1/128,10.42.0.0/16"
```

## 安全注意

:::danger
- 如果 clewdr-hub **直接暴露在公网**（没有任何反代），把 `trusted_proxies` 设为 `[]`，强制忽略所有转发头，避免直连攻击者伪造 IP。
- `trusted_proxies` 列表中的来源**完整信任**其 `X-Forwarded-For`，请只填你自己控制的反代来源段。
:::

**验证方法**：管理后台请求详情里 `client_ip` 应该是你真实出口 IP，而不是 `127.0.0.1` 或 `172.x.x.x`。
