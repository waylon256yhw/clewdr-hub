# Release Notes

## v1.0.3

### 新增

- 管理后台账号池新增 OAuth 账号接入流程，支持生成 Claude 授权 URL、粘贴 callback URL 或 code 完成入库。
- 账号模型新增 OAuth 凭据、过期时间、刷新时间、错误状态和运行时用量展示。
- 新增纯 OAuth 账号池调度路径，支持不依赖 cookie 的 Bearer API 直连调用。

### 变更

- 收敛发往 Anthropic 的请求头策略，恢复 `anthropic-beta` 客户端透传合并，移除多余硬编码伪装头。
- 保留 `User-Agent` 与最小必需版本头，降低与真实 CLI/SDK 行为冲突的概率。
- browser emulation 继续限定在 cookie / browser 流程；纯 OAuth 运行态保持轻量 API 调用。
- 清理已废弃 stealth 配置项：`cc_sdk_version`、`cc_node_version`、`cc_stainless_os`、`cc_stainless_arch`、`cc_beta_flags`。

### 修复

- 修复管理端 OAuth token 兑换解析失败问题，对齐 Anthropic 实际 token 端点请求格式。
- 修复 OAuth state 回填链路，支持完整 callback URL、`code#state` 和仅 code 输入。
- 修复纯 OAuth 账号运行态未继承代理配置的问题，避免与 cookie 路径出口不一致。
- 修复纯 OAuth 账号认证失败后仍被反复选中的问题；现在会写回 `last_error` 并标记为 `auth_error`。
- 优化账号池添加弹窗 UI，移除冗余字段与重复按钮，压缩长授权 URL 展示。
