### 教程：在宝塔面板上部署 clewdr-hub

#### 1. 打开宝塔终端

登录宝塔 → 左侧 **终端** → 切到 root：

```bash
sudo -i
```

#### 2. 一键安装

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/install.sh | bash
```

会自动下载匹配版本并通过 systemd 注册开机自启。

#### 3. 放行端口

宝塔 **安全** 页面放行 `8484`（默认端口）。

#### 4. 访问管理后台

浏览器打开 `http://你的服务器IP:8484`，默认密码 `password`，首次登录强制改密。

#### 5. 初始配置

后台按这个顺序操作：

1. **账号池** → 添加 Cookie 或 OAuth。试用、促销账号勾「优先消耗」让调度器先把它榨干再用其它账号。
2. **用户** → 创建团队成员，分配策略（并发/RPM/预算）。
3. **API Keys** → 给每个用户生成一个 Key。
4. 客户端配置：

   ```bash
   export ANTHROPIC_BASE_URL=http://你的服务器IP:8484
   export ANTHROPIC_API_KEY=sk-...
   ```

#### 日常管理

回 SSH 终端：

```bash
clewdr menu       # 交互菜单：改密码、导入导出配置、检查更新
clewdr            # 看运行状态
clewdr update     # 升级到最新版
```

systemd 控制：

```bash
systemctl status clewdr      # 服务状态
systemctl restart clewdr     # 重启（改完配置后用）
journalctl -u clewdr -f      # 实时日志
```

#### 卸载

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/uninstall.sh | bash
```

加 `--purge` 同时清掉配置和数据库（会要求 TTY 输入 `yes` 二次确认）：

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/uninstall.sh | bash -s -- --purge
```

#### 注意事项

- **端口冲突**：8484 被占用就改 `/opt/clewdr/clewdr.toml` 里的 `port`，然后 `systemctl restart clewdr`。
- **防火墙**：除宝塔 **安全** 外，云厂商的安全组也要放行端口。
- **数据备份**：复制 `/opt/clewdr/clewdr.db` 即可；或用 `clewdr menu` → 导出配置生成加密备份包。
