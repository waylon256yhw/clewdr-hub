---
title: 第一个请求
description: 从后台登录到客户端配置的完整流程，以及第一个 API 请求示例。
sidebar:
  order: 4
---

## 完整流程

```
后台登录
 → （可选）先到代理页添加备用代理
 → 账号池添加 Cookie / OAuth / API Key 账号并按需绑定代理
 → 创建 API Key
 → 客户端配置下面两行
```

单人到这里就够了。多人团队还要加[用户与策略](../../guides/teams/)。

## 配置客户端

```bash
export ANTHROPIC_BASE_URL=http://你的IP:8484
export ANTHROPIC_API_KEY=sk-...    # 从后台 API Keys 页创建
```

配好后，官方 Claude Code、任意 Anthropic SDK 客户端都能直接用——clewdr-hub 不做用户端 UA 校验，自由接入。

## 发一个请求

原生 Anthropic Messages API：

```bash
curl http://你的IP:8484/v1/messages \
  -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{
    "model": "claude-sonnet-4-6",
    "max_tokens": 256,
    "messages": [{"role": "user", "content": "Hi"}]
  }'
```

也可以用 OpenAI 风格的客户端，见 [OpenAI 兼容入口](../../guides/openai-compat/)：

```python
from openai import OpenAI

client = OpenAI(base_url="http://你的IP:8484/v1", api_key="sk-...")
resp = client.chat.completions.create(
    model="claude-sonnet-4-6",
    messages=[{"role": "user", "content": "Hi"}],
)
print(resp.choices[0].message.content)
```

## 看请求记录

后台 **日志** 页能看到每条请求的 token / 费用 / 耗时 / 状态，点开有详情；**运维** 页有累计用量和趋势。详见[管理后台导览](../../guides/admin-console/)。

## 日常管理

```bash
clewdr menu       # 交互式管理菜单
clewdr            # 一眼看运行状态
clewdr update     # 升级到最新版
```

完整命令见 [CLI / clewdr menu](../../reference/cli/)。
