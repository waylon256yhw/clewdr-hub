---
title: 前端
description: React 19 + Mantine 9 + TanStack Query，构建产物嵌入二进制；运维页数据口径与前后端集成模式。
sidebar:
  order: 9
---

React 19 + Mantine 9 + TanStack Query，构建产物输出到 `static/` 目录。

## 构建

```bash
cd frontend && npm ci && npm run build
```

产物约 640KB JS + 210KB CSS（gzip 后 ~225KB）。后续可做路由级拆包优化，当前优先级不高。

开发期的 Vite HMR、代理配置见[开发环境 · 前端独立开发](../environment/#前端独立开发)。

## 运维页数据口径

- 路由：`/ops`
- API：`GET /api/admin/ops/usage?range=24h|7d|30d&top_users=...&user_id=...`
- 累计卡片：来自 `usage_lifetime_totals`（不受 `request_logs` 7 天清理影响）
- 图表窗口：来自 `request_logs`，按 `Asia/Shanghai`（UTC+8）按小时/天分桶
- 自动刷新：`refetchInterval=60_000`，并叠加 `/api/admin/events` 的 SSE invalidation

## 与后端集成

两种模式，互斥：

- **embed-resource** feature（Docker/Release）：`static/` 编译嵌入二进制，`include_dir!` 宏
- **external-resource** feature（开发）：运行时从文件系统读 `static/` 目录

`Cargo.toml` 默认 `default = ["portable", "external-resource"]`。
