### 教程：在宝塔面板上部署 clewdr-hub

#### 前置步骤：打开宝塔面板的终端
1. 登录到你的宝塔面板。
2. 在左侧导航栏中点击 **终端**。
3. 确保已切换到 root 用户（`sudo -i`）。

#### 步骤 1：查看系统架构

```bash
uname -m
```

- `x86_64`：下载 `clewdr-linux-x86_64.zip`
- `aarch64`：下载 `clewdr-linux-aarch64.zip`

#### 步骤 2：确认 glibc 版本

```bash
ldd --version
```

记录第一行输出的版本号。如果版本低于 clewdr-hub 构建要求（通常 >= 2.38），请改用 musl 静态链接版本。

#### 步骤 3：下载并解压

```bash
# 示例：glibc 版本足够，下载普通版本（x86_64）
curl -fL https://github.com/waylon256yhw/clewdr-hub/releases/latest/download/clewdr-linux-x86_64.zip -o clewdr.zip

# 如果 glibc 版本不足，改用 musl 版本
# curl -fL https://github.com/waylon256yhw/clewdr-hub/releases/latest/download/clewdr-musllinux-x86_64.zip -o clewdr.zip

unzip clewdr.zip -d .
chmod +x clewdr
```

运行测试：

```bash
./clewdr
```

如果出现 `GLIBC_X.XX not found` 错误，说明需要改用 musl 版本。

#### 步骤 4：通过宝塔面板配置项目

1. 打开左侧导航栏 **网站** → 上方 **其他项目** → **添加通用项目**。
2. **项目执行文件**：填写 clewdr 的完整路径（如 `/root/clewdr`）。
3. **项目名称**：`clewdr-hub`。
4. **项目端口**：默认 `8484`。
5. **执行命令**：同执行文件路径。
6. **运行用户**：`root`。
7. 建议开启 **开机启动**。
8. 点击 **确定**。

#### 步骤 5：访问管理后台

1. 在 **其他项目** 列表中启动 clewdr-hub。
2. 浏览器访问 `http://你的服务器IP:8484`。
3. 默认管理员密码 `password`，首次登录会强制改密。

#### 步骤 6：初始配置

1. **账号池** → 添加 Cookie（支持 Cookie 和 OAuth 两种方式）。
2. **用户** → 为团队成员创建账号，分配策略（并发/RPM/预算限额）。
3. **API Keys** → 为每个用户创建 Key。
4. 客户端配置：

   ```bash
   export ANTHROPIC_BASE_URL=http://你的服务器IP:8484
   export ANTHROPIC_API_KEY=sk-...    # 从后台创建的 Key
   ```

   同一台机器上的服务可以用 `http://127.0.0.1:8484`。

#### 注意事项

- **优先选择 musl 版本**：除非明确知道系统 glibc 满足要求。
- **架构匹配**：确保下载文件的架构与 `uname -m` 输出一致。
- **端口冲突**：如果 8484 被占用，通过环境变量 `CLEWDR_PORT` 修改。
- **防火墙**：确保端口已在宝塔 **安全** 中放行。
- **数据库**：SQLite 数据库自动创建在运行目录下（`clewdr.db`），备份时复制此文件即可。
