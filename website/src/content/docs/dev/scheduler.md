---
title: 账号调度器
description: AccountPoolActor（ractor）的状态、消息类型、并发槽、脏刷盘与「优先消耗」调度规则。
sidebar:
  order: 4
---

核心是 `AccountPoolActor`（基于 [ractor](https://github.com/slawlor/ractor) 框架的 actor 模型）。

## 状态

```rust
struct AccountPoolState {
    valid: VecDeque<AccountSlot>,           // 可用队列
    exhausted: HashSet<AccountSlot>,        // 冷却中（429）
    invalid: HashSet<InvalidAccountSlot>,   // 失效（auth error）
    moka: Cache<u64, i64>,                  // 亲和性缓存：affinity hash -> account_id（1h TTL）
    inflight: HashMap<i64, (u32, u32)>,     // account_id → (当前并发, max_slots)
    dirty: HashSet<i64>,                    // 待刷盘的 account_id
    db: SqlitePool,
    probing: HashSet<i64>,                  // 正在探测中的 account_id
    reactivated: HashSet<i64>,              // 本轮被重激活的 account_id
    probe_errors: HashMap<i64, String>,     // 探测失败信息
    drain_first_ids: HashSet<i64>,          // 标记为优先消耗的 account_id
}
```

## 消息类型

| 消息 | 触发方 | 作用 |
|------|--------|------|
| `Request` | 请求处理器 | dispatch 一个账号，inflight++ |
| `Return` | 请求结束 | 回收账号（更新 or 移入 exhausted/invalid） |
| `ReleaseSlot` | 请求结束 | inflight-- |
| `Submit` | admin API | 添加新账号 |
| `CheckReset` | 定时器 (5min) | 检查 exhausted 账号是否可以恢复 |
| `FlushDirty` | 定时器 (15s) | 批量写脏数据到 DB |
| `ReloadFromDb` | admin API | 重新加载（保留 inflight 计数） |
| `ProbeAccounts` | admin API | 探测指定账号列表 |
| `BeginProbe` | 探测流程 | 标记账号进入探测状态 |
| `ClearProbing` | 探测完成 | 清除探测状态标记 |
| `SetProbeError` / `ClearProbeError` | 探测流程 | 记录/清除探测错误信息 |
| `GetProbingIds` / `GetProbeErrors` | admin API | 查询探测状态 |

## 并发槽（max_slots）

每个账号有独立的 inflight 计数器。`dispatch()` 跳过 `inflight >= max_slots` 的账号。释放路径：

- **非流式请求**：handler 中显式调用 `release_slot()`
- **流式请求正常结束**：`MessageStop` 事件的 spawn 中释放，用 `AtomicBool` 防重复
- **流式请求异常终止**：`SlotDropGuard` 的 `Drop` 通过 `tokio::spawn` 异步释放
- **reload**：保留已有 inflight 计数，只更新 max_slots 值

## 脏刷盘

每 15 秒批量 upsert `account_runtime_state` 表（使用 SQLite transaction），只写 dirty set 中的账号。shutdown 时 `post_stop()` 强制刷全量。

## drain_first（优先消耗）

账号级的布尔开关，用于把「限量/试用/促销」账号优先榨干再动主池。用户侧用法见[账号池 · 优先消耗](../../guides/account-pool/#优先消耗drain_first)。行为规则：

- **亲和 key**（`middleware/claude/request.rs::request_affinity_hash`）：官方 Claude Code 会透传稳定的 `metadata.user_id`，其中包含 `_session_<uuid>`；调度优先用它作为会话级亲和锚点，保证同一 CLI session 内的 Opus 主请求和 Haiku 辅助请求即使 system prompt / cache-control blocks 不一致，也会落到同一个账号。若请求没有客户端传入的 session metadata（例如 2API 由服务端补注入随机 session），则不使用该随机值，回退到原来的 cache-control system blocks 哈希。
- **Cookie 池**（`services/account_pool.rs::dispatch`）：调度先查 moka 亲和性缓存。缓存命中且目标账号满足 `bound` 约束、仍有空闲槽时，继续返回同一个 `account_id`，即使它属于 `drain_first` 池。若缓存账号只是满槽，则临时借用其他可用账号（优先借 `drain_first` 兄弟账号），但不改写缓存；只有缓存账号已失效、被删除或不在本次 `bound` 约束内时才清掉缓存并重新绑定。无有效缓存时才进入 `drain_first` 优先选择，再落回普通 round-robin。
- **OAuth 池**（`providers/claude/mod.rs::OAuthAccountPool::acquire`）：将账号**分区**成 drain 和普通两个子集，每个子集维护**独立**的 round-robin 游标（`drain_cursor` / `normal_cursor`）。优先在 drain 子集里按 RR 取可用账号；全部饱和时才在普通子集按 RR 降级。独立游标是为了避免 drain 账号的释放/重取把普通子集的 RR 位置反复拉回头部，造成某一个普通账号（通常是 `rr_order` 最小的）被集中点名。
- **索引重建**：`drain_first_ids` 在每次账号池刷新（`ReloadFromDb` / 启动时 `init`）时从 DB 重建，管理后台勾选后立即生效，无需重启。
- **持久化**：`accounts.drain_first` 列（`INTEGER NOT NULL DEFAULT 0` + CHECK）+ 部分索引 `idx_accounts_drain_first WHERE drain_first = 1`（目前未被 SQL 查询使用，保留为后续潜在批查询做准备）。
- **回收**：冷却（429）或失效（auth error）发生时，和普通账号走同一套 release/exhausted 路径，`drain_first` 标记不影响冷却恢复逻辑。
- **与 `bound` 的关系**：账号池会把 `bound` 集合纳入最终 moka key；命中后仍会校验 `bound`。`bound` 约束先于 `drain_first` 优先级——绑定到普通账号 A 的 API Key 不会被改派到 drain 账号 B。
- **默认值**：`false`；存量部署升级迁移后行为完全不变。
