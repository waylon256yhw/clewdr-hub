---
title: 安装部署
description: clewdr-hub 的多种部署方式——一键脚本、Docker Compose、手动安装、Hugging Face Space、宝塔面板，以及卸载。
sidebar:
  order: 3
---

挑一种适合你的方式。最快的是[一键安装](#一键安装推荐)；想快速体验可用 [Hugging Face Space](#hugging-face-space)。

所有方式装完后，管理后台都在 `http://你的IP:8484`。未预设 `ADMIN_PASSWORD` 时，首次启动的随机初始密码会打印在服务日志中，**首次登录强制改密**。

## 一键安装（推荐）

Linux / macOS / Termux 一条命令装好，自动注册开机自启：

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/install.sh | bash
```

装完直接打开管理菜单，按编号选择——查看状态、改密码、导入导出配置都在里面：

```bash
clewdr menu
```

### Termux 用户

要在关掉 Termux 后服务仍在线，先到 F-Droid 装一次 [Termux:Boot](https://f-droid.org/packages/com.termux.boot/) 并打开它，再执行：

```bash
clewdr service install
```

## Docker Compose

```bash
mkdir clewdr-hub && cd clewdr-hub
curl -O https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/docker-compose.yml
docker compose up -d
```

数据持久化在 Docker volume `clewdr-data` 中，`docker compose down` 不会丢数据。

## 手动安装

到 [Releases](https://github.com/waylon256yhw/clewdr-hub/releases/latest) 下载对应平台的 zip，解压加可执行权限即可。要开机自启可以接着跑 `clewdr service install`。

## Hugging Face Space

无需本地编译，直接基于预构建镜像。

1. 前往 [HF Space](https://hf.space)，点击 **New space**，名称随意，Space SDK 选 **Docker**，按需选可见性，点创建。
2. 下载 [Dockerfile.huggingface](https://github.com/waylon256yhw/clewdr-hub/blob/master/Dockerfile.huggingface)，重命名为 `Dockerfile`，上传到 Space 的 Files：

   ```dockerfile
   FROM ghcr.io/waylon256yhw/clewdr-hub:latest

   ENV CLEWDR_IP=0.0.0.0
   ENV CLEWDR_PORT=${PORT:-7860}
   ENV CLEWDR_NO_FS=TRUE

   EXPOSE ${PORT:-7860}
   ```

3. 在 **Settings → Variables and secrets** 配置 `ADMIN_PASSWORD`（建议放 Secrets）。其他配置都通过管理后台操作，不需要环境变量。
4. 状态变为 **Running** 后，打开 Space 页面就是管理后台。API 地址即 `https://你的用户名-space名称.hf.space`。
5. 更新：**Settings → Factory rebuild** 拉取最新镜像。

:::caution
HF Space 模式启用了 `CLEWDR_NO_FS=TRUE`，配置和数据库在内存中，**Space 重启后数据会丢失**。需要持久化请用 Docker Compose 或二进制部署。免费 tier 无流量时会休眠，唤醒需几秒。
:::

## 宝塔面板

1. 宝塔 → 左侧 **终端** → `sudo -i` 切到 root。
2. 跑[一键安装](#一键安装推荐)命令，会自动下载匹配版本并通过 systemd 注册开机自启。
3. 宝塔 **安全** 页面放行 `8484`（默认端口）。**注意**：除宝塔安全外，云厂商的安全组也要放行端口。
4. 浏览器打开 `http://你的服务器IP:8484` 登录，按[第一个请求](../first-request/)初始化。

systemd 控制：

```bash
systemctl status clewdr      # 服务状态
systemctl restart clewdr     # 重启（改完配置后用）
journalctl -u clewdr -f      # 实时日志
```

:::tip
端口冲突就改 `/opt/clewdr/clewdr.toml` 里的 `port` 再 `systemctl restart clewdr`。数据备份直接复制 `/opt/clewdr/clewdr.db`，或用 `clewdr menu` → 导出配置生成加密备份包。
:::

## 卸载

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/uninstall.sh | bash
```

默认只删二进制和服务注册，**保留**配置和数据。要把配置和数据库也删掉，加 `--purge`（会要求 TTY 输入 `yes` 二次确认）：

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/uninstall.sh | bash -s -- --purge
```

## 环境变量

监听地址、端口、管理员密码、信任反代等都能用环境变量覆盖，见[环境变量](../../reference/environment-variables/)。
