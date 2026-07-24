---
title: 认证体系
description: API Key 认证（blake3）、Admin Session（HMAC 签名 cookie），以及 plaintext_key 的设计取舍。
sidebar:
  order: 5
---

## API Key 认证（用户请求）

Key 格式：`sk-{lookup_prefix}_{random_part}`

- `lookup_prefix`（8 字符）：明文存储，用于快速查找
- 完整 key 的 blake3 哈希存储在 `key_hash` 列
- 认证流程：按 prefix 查行 → 计算提交 key 的 blake3 → 比对

## Admin Session（后台登录）

- admin 密码用 argon2 哈希存储
- 登录成功后签发 HMAC-SHA256 签名 cookie（`clewdr_session`）
- payload：`{user_id}.{session_version}.{expires}`
- 默认 TTL 24h（可通过 `admin_session_ttl_hours` 调整），不会自动续期
- `session_version` 递增即可踢掉所有会话；修改密码和“退出所有设备”都会执行此操作
- Release 首次启动生成随机初始密码；`must_change_password` 在后端限制管理 API，而不只依赖前端弹窗

## 关于 plaintext_key

**已知设计决策**：`api_keys` 表保留了 `plaintext_key` 列，admin 后台可以回看完整 key。

这是有意为之——在高信任小团队场景下，admin 需要能帮成员找回 key，避免「忘了复制就永远丢失」的运维痛点。trade-off 是 DB 泄露等同于所有 key 泄露。如果你的威胁模型需要更强的隔离，可以：

1. 删除 migration 中的 `plaintext_key` 列
2. 移除 `create_api_key()` 中的 `.bind(&plaintext)`
3. admin key 列表不再显示完整 key

更多设计取舍见[已知问题与设计决策](../design-decisions/)。
