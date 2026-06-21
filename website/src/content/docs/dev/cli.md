---
title: CLI 与 TUI 菜单
description: 子命令分发契约、裸命令短路、inquire TUI 菜单，以及如何新增一个菜单项。
sidebar:
  order: 8
---

用户视角的命令见[参考 · CLI / clewdr menu](../../reference/cli/)。本页讲实现契约。

## 子命令分发

`src/cli/{status,diagnose,reset,export,import,service,menu}.rs` 每个文件遵循统一契约：`pub struct Args`（`#[derive(clap::Args)]`，挂在 `crate::Command` 下）+ `pub async fn run(Args) -> Result<(), ClewdrError>`。

`cli::dispatch` 在 `main.rs` 里 **早于** logging 和 `CLEWDR_CONFIG` 触发——`ClewdrConfig::new()` 会 spawn 一个回写 `clewdr.toml` 的 task，与 `import-config` 的事务化导入抢同一个文件。前置 dispatch 是规避这个竞争的关键。

`cli/bundle.rs` 与 `cli/crypto.rs` 是 export/import 共享工具（表清单、KDF+AEAD），不直接对应 verb。

## 裸命令短路

`should_show_interactive_default`（`main.rs:144`）：TTY + 无参 + 没显式 `--update` → 走 `interactive_default()`，打印版本 + status 报告 + 常用子命令清单。非 TTY 仍走 `serve`，systemd 单元和 `docker run` 默认入口都在这条路径上，向后兼容不变。

## TUI 菜单（feature = "tui"，默认开启）

`cli/menu.rs` 是 inquire 包出来的薄壳，**不承载任何业务逻辑**。每个菜单项：用 `Text` / `Confirm` / `Select` 收集参数 → 构造同名 verb 的 `Args` → 调 `cli::*::run()`。原则：verb 加 flag 时同步在菜单里暴露，不要 fork 实现。

行为细节：

- 非 TTY（`stdin` 或 `stdout` 任一不是终端）直接拒绝，并给出 `clewdr --help` 提示——避免 inquire 的 crossterm 后端报模糊 IO 错误
- Esc/Ctrl-C 在子提示里退回主菜单（`wrap_or_cancel` 处理 `OperationCanceled` / `OperationInterrupted`），在主菜单上则退出程序
- `diagnose` 用非退出版 `run_and_report`，正常 `run()` 会 `process::exit(1)` 把菜单循环也炸掉
- `import` 选「恢复」模式时强制再来一次 Confirm，因为这一步会清表

## 加一个新菜单项

1. `src/cli/foo.rs`：`Args` + `run`
2. `src/cli/mod.rs`：`pub mod foo;` + `dispatch` 加一臂
3. `src/lib.rs::Command`：加 `Foo(cli::foo::Args)` 变体
4. `src/cli/menu.rs`：
   - `MenuAction` 加变体
   - `menu_entries()` 加带编号的标签
   - `run_action` 加分发
   - `menu_foo` 用 `Text/Confirm/Select::new` + `wrap_or_cancel` 收参，然后调 `cli::foo::run()`
5. 更新 `menu_entries_include_quit_and_main_verbs` 单测里的标签数组
