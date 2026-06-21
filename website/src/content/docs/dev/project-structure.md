---
title: 项目结构
description: clewdr-hub 后端（Rust）与前端（React）的目录结构。
sidebar:
  order: 2
---

## 后端 `src/`

```
src/
├── main.rs                    # 入口：CLI 参数、DB 初始化、启动 HTTP server
├── lib.rs                     # 模块注册 + Command 枚举
├── cli/                       # 子命令处理
│   ├── mod.rs                 # dispatch：早于 logging / CLEWDR_CONFIG 触发
│   ├── status.rs              # status：toml + HTTP 探针
│   ├── diagnose.rs            # diagnose：10 项只读体检（FAIL → 退出码 1）
│   ├── reset.rs               # reset-admin-password
│   ├── export.rs              # export-config（默认 Argon2id+AES-256-GCM）
│   ├── import.rs              # import-config（merge/restore，单事务）
│   ├── service.rs             # service install/uninstall（systemd / Termux:Boot）
│   ├── menu.rs                # menu：inquire 薄壳（feature = "tui"）
│   ├── bundle.rs              # export/import 共享：表清单 + 字段编码
│   └── crypto.rs              # export/import 共享：KDF + AEAD 包装
├── config/                    # 配置、常量、AccountSlot 结构体
│   ├── constants.rs           # DB_PATH / CONFIG_PATH / 全局 LazyLock
│   └── cookie.rs              # AccountSlot：账号运行时状态（token、用量、窗口）
├── db/
│   ├── mod.rs                 # init_pool / seed_admin / migrations
│   ├── models.rs              # AuthenticatedUser 等共享类型
│   ├── accounts.rs            # AccountWithRuntime / load_all_accounts / batch_upsert
│   ├── proxies.rs             # ProxyRow / build_proxy_url / update_proxy_test_result
│   ├── queries.rs             # authenticate_api_key / touch_api_key / touch_user
│   ├── api_key.rs             # Key 生成（blake3 哈希 + lookup 前缀）
│   └── billing.rs             # insert_request_log / upsert_usage_rollup / upsert_usage_lifetime_total
├── api/
│   ├── claude_code.rs         # POST /v1/messages 主处理器
│   ├── models.rs              # GET /v1/models
│   ├── health.rs              # GET /health
│   ├── auth.rs                # POST /auth/login, /auth/logout
│   └── admin/                 # /api/admin/* 管理 API（accounts, proxies, users, keys, policies, ops...）
├── middleware/
│   ├── auth.rs                # RequireFlexibleAuth（API Key）/ RequireAdminAuth（session cookie）
│   └── claude/request.rs      # ClaudeCodePreprocess：请求预处理、billing header 注入
├── providers/claude/mod.rs    # ClaudeProvider：构建 ClaudeCodeState 并调用 try_chat
├── claude_code_state/
│   ├── mod.rs                 # ClaudeCodeState：持有账号、client、billing context
│   ├── chat.rs                # try_chat / try_count_tokens / 流式转发 / 用量持久化
│   ├── exchange.rs            # OAuth token 交换（cookie → access_token）
│   ├── organization.rs        # 获取组织 UUID
│   └── probe.rs               # 账号探测（类型、邮箱、用量窗口）
├── services/
│   ├── account_pool.rs        # AccountPoolActor（ractor）：调度、回收、inflight 追踪、脏刷盘
│   ├── user_limiter.rs        # UserLimiterMap：per-user 并发 semaphore
│   └── log_rotation.rs        # 日志轮转（默认保留 7 天）
├── router.rs                  # RouterBuilder：路由注册、中间件挂载
├── session.rs                 # HMAC-SHA256 签名 cookie（创建/验证/过期）
├── stealth.rs                 # 伪装 profile（CLI/SDK 版本、请求头构建）
├── billing.rs                 # BillingContext / persist_billing_to_db
├── error.rs                   # ClewdrError（snafu 派生）
└── types/claude.rs            # Claude API 请求/响应类型定义
```

## 前端 `frontend/src/`

```
frontend/src/
├── main.tsx                   # React 入口
├── api.ts                     # API client + TypeScript 接口定义
├── routes/                    # 页面组件（Dashboard, Ops, Accounts, Proxies, Users, Keys, Logs, Settings）
└── lib/                       # 工具函数
```

前端技术栈与构建见[前端](../frontend/)。
