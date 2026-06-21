---
title: CLI / clewdr menu
description: clewdr 命令行的常用子命令——状态、菜单、更新、服务安装、诊断、配置导入导出。
sidebar:
  order: 5
---

安装后，`clewdr` 命令提供日常运维入口。开发者视角的子命令分发契约见[开发 · CLI 与 TUI 菜单](../../dev/cli/)。

## 常用命令

```bash
clewdr            # 一眼看运行状态（裸命令，TTY 下还会列常用子命令）
clewdr menu       # 交互式管理菜单
clewdr update     # 升级到最新版
```

非 TTY 环境（systemd 单元、`docker run` 默认入口）下，裸 `clewdr` 走的是 `serve`，正常启动服务。

## clewdr menu 能做什么

`clewdr menu` 是一个交互式薄壳，把下面这些操作收进编号菜单：

| 操作 | 说明 |
|------|------|
| 查看状态 | toml + HTTP 探针 |
| 查看诊断 | 10 项只读体检 |
| 重置密码 | 重置管理员密码 |
| 导出配置 | 默认 Argon2id + AES-256-GCM 加密备份 |
| 导入配置 | merge / restore（恢复模式会清表，需二次确认） |
| 安装 / 卸载服务 | systemd / Termux:Boot |
| 检查更新 | 拉取最新版本 |

:::note
`clewdr menu` 需要 TTY；非终端环境会直接拒绝并提示 `clewdr --help`。
:::

## 服务管理

```bash
clewdr service install     # 注册开机自启（systemd / Termux:Boot）
clewdr service uninstall   # 取消开机自启
```

systemd 下也可以直接用 `systemctl` / `journalctl`，见[宝塔面板部署](../../start/installation/#宝塔面板)。

## 诊断

```bash
clewdr diagnose            # 10 项只读体检，FAIL 时退出码为 1
```

适合接入监控或安装后自检。
