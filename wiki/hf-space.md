### 教程：将 clewdr-hub 部署到 Hugging Face Space

#### 步骤 1：创建 Space

1. 前往 [HF Space](https://hf.space)，点击 **New space**。
2. 填写 Space 名称，Space SDK 选择 **Docker**，可见性按需选择。
3. 点击创建。

#### 步骤 2：上传 Dockerfile

1. 下载 [Dockerfile.huggingface](https://github.com/waylon256yhw/clewdr-hub/blob/master/Dockerfile.huggingface)。
2. 重命名为 `Dockerfile`。
3. 上传到 Space 的 Files 中。

`Dockerfile.huggingface` 内容如下，直接基于预构建镜像，无需本地编译：

```dockerfile
FROM ghcr.io/waylon256yhw/clewdr-hub:latest

ENV CLEWDR_IP=0.0.0.0
ENV CLEWDR_PORT=${PORT:-7860}
ENV CLEWDR_NO_FS=TRUE

EXPOSE ${PORT:-7860}
```

#### 步骤 3：配置环境变量

在 Space 的 **Settings → Variables and secrets** 中配置：

| 变量 | 建议位置 | 说明 |
|------|----------|------|
| `ADMIN_PASSWORD` | Secrets | 管理员密码（首次登录后强制修改） |

其他配置通过管理后台操作，不需要环境变量。

#### 步骤 4：等待构建完成

状态变为 **Running** 后即可访问。打开 Space 页面就是管理后台。

#### 步骤 5：初始配置

1. 使用 `ADMIN_PASSWORD` 设置的密码登录管理后台（默认 `password`，首次登录强制改密）。
2. **账号池** → 添加 Cookie 或 OAuth 账号。限量试用/促销账号可勾选「优先消耗」，调度器会优先打满它再降级到其它账号。
3. **用户** → 创建团队成员，分配策略。
4. **API Keys** → 为每个用户创建 Key。

#### 步骤 6：客户端配置

API 地址为 Space 的 URL：

```
https://你的用户名-space名称.hf.space
```

客户端配置：

```bash
export ANTHROPIC_BASE_URL=https://你的用户名-space名称.hf.space
export ANTHROPIC_API_KEY=sk-...    # 从后台创建的 Key
```

#### 更新

前往 Space 的 **Settings → Factory rebuild** 点击按钮即可拉取最新镜像。

#### 注意事项

- HF Space 模式下启用 `CLEWDR_NO_FS=TRUE`，配置和数据库存储在内存中，Space 重启后数据会丢失。
- 如果需要持久化数据，建议使用 Docker Compose 或二进制部署方式。
- HF Space 免费 tier 会在无流量时休眠，唤醒需要几秒。
