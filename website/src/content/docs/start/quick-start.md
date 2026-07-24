---
title: 快速开始
description: 一条命令装好 clewdr-hub，5 分钟内发出第一个请求。
sidebar:
  order: 2
---

单人自用从安装到第一个请求只要几分钟。需要更多部署方式（Docker / 宝塔 / Hugging Face）见[安装部署](../installation/)。

## 1. 一键安装

Linux / macOS / Termux 一条命令装好，自动注册开机自启：

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/install.sh | bash
```

## 2. 打开管理后台

浏览器访问 `http://你的IP:8484`。首次启动的随机初始密码会打印在服务日志中，**首次登录强制改密**。

命令行也能随时查看状态、改密、导入导出配置：

```bash
clewdr menu
```

## 3. 添加账号并创建 Key

后台按这个顺序操作：

1. **账号池** → 添加 Cookie / OAuth / API Key 账号（详见[账号池](../../guides/account-pool/)）。
2. **API Keys** → 创建一个 Key。

> 多人团队还要配置用户和策略，见[团队管理](../../guides/teams/)。

## 4. 配置客户端

```bash
export ANTHROPIC_BASE_URL=http://你的IP:8484
export ANTHROPIC_API_KEY=sk-...    # 第 3 步从后台创建
```

完整的「登录 → 加代理 → 加账号 → 建 Key → 配客户端」流程和第一个请求示例见[第一个请求](../first-request/)。

## 下一步

- [安装部署](../installation/)：Docker Compose、宝塔面板、Hugging Face Space、手动安装、卸载。
- [管理后台导览](../../guides/admin-console/)：每个页面能做什么。
- [账号池](../../guides/account-pool/)：三类上游账号、优先消耗、绑定代理。
