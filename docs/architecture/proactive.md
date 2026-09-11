# 主动性

oc 能在你没说话的时候自己动。三个来源：定时任务（cron）、话题触发式待办（standing intent）、唤醒（wake）。全部挂在同一个心跳底座上。

## 心跳底座

`Scheduler` 按 `heartbeat_secs`（默认 60s）跑一次 tick，每次 tick 串行处理几件事：

- 卡死会话扫描（`queue::diagnose` → abort-drain 释放车道）
- dreaming 触发判定（`dreaming_gate`）
- cron 到期扫描（最小堆 `next_at ≤ now`）
- GC（幂等表 TTL 清扫、空闲会话淘汰）

心跳串行 `await` 每个 tick，不自我并发——这避免了上一轮还没跑完下一轮又起来的重入问题。

## cron：定时任务

```
cron_heap 到期（next_at ≤ now）
  ├─ proactive::allow_fire 检查 cooldown / budget / expiry → 拒则跳过 + 重排
  ├─ 起隔离子会话（SessionKind::Cron），独立 timer + 到期 abort + 清理
  ├─ 跑一轮 agent（注入 cron.prompt）
  ├─ 结果 → Event::Proactive{kind: Reminder} → 推给所有 client
  └─ 更新 last_fired_at / fired_count，core::next_fire 算下次 → 重排堆
```

隔离子会话意味着定时提醒不占用你 `main` 会话的车道。

### 模型可直接调用

`cron` 工具带四个 op：`add`（重复任务）、`delay`（一次性延时）、`list`、`rm`。系统提示词里明确告诉模型「登记后系统会主动推送，不占用当前对话，无需等待」，并禁止用 shell 睡眠或反复查时间来等待。

这段措辞针对的是一个具体的模型错误前提——它原本以为「必须自己等着才能提醒」，于是会去调 `sys:now` + `Start-Sleep` 硬凑等待，最后撞上 loop detection。

### 一次性延时走另一条路

cron 表达式最小粒度是分钟，「10 秒后提醒我」没法表达。`delay` 落库时 `expr` 记 `@once` 标记、`next_at` 存绝对秒，触发后即删。

≤1h 的近端任务另挂**精确 tokio timer**，不等心跳——心跳是分钟级的，短延时靠扫描会迟到近一分钟。

### 两条触发路径的去重

一次性任务同时有 timer 和心跳兜底两条路径，用 `DELETE` 的 rows-affected 做原子认领（先删后跑），保证只触发一次。

重复任务只有心跳一条路径，并且刻意保持**先跑后重排**的顺序——如果先占位再跑，进程在中途被杀会把 `next_at` 永久留空，一条每日提醒从此再不触发。那比偶尔重复提醒一次糟得多。

### 时区

`next_fire(expr, after, tz)` 按 IANA 时区名解释表达式里的时分。时区名由 CLI 探测本机后传入（`time` crate 取本地偏移在多线程进程里不可靠，daemon 不能自己探）。

时区名拼错会**报错而非静默按 UTC**——静默降级正是这类缺陷难以被发现的原因。

`oc cron list` 的输出按每行自己的时区渲染成本地墙上时间，并附剩余时长。

### 已知语义偏差

标准 cron 在 day-of-month 和 day-of-week 都非 `*` 时取 OR，当前实现取 AND。个人助手场景多数只用其一，见 [看板](../../BOARD.md) BUG-1。

## standing intent：话题触发式待办

与 cron 的区别：cron 到点触发，intent 由**话题命中**触发。

```
每条入站消息
  core::intent_prefilter（关键词命中）
    → proactive::allow_fire（anti-nagging）
        ├─ 允许：注入隐藏上下文（提醒 agent 有这条待办）+ fired_count++
        └─ 拒绝：静默跳过
```

例：登记「聊到出差/德国时提醒我带转换插头」，之后任何一轮对话提到这些词就会触发。

触发链挂在 `oc-server::session::intent_scan` 的 `begin_run`（主会话入站路径）。cron 和 dreaming 走隔离子会话，天然不经过这条路径，不会误注入。

预筛当前是**纯词法** `contains`，与 Lane1 一致。向量语义预筛等 `sqlite-vec` 接入后再议。

注入复用 Lane1 的 bootstrap 管线，文本标注「待办提醒」以便与记忆区分。

创建入口是 `oc intent add`（显式创建）。不做「从对话里自动解析待办」——参照项目验证过这条路很脆弱。

## anti-nagging：别唠叨

`proactive::allow_fire` 是纯函数，判定三个维度：

| 维度 | 默认 | 说明 |
|---|---|---|
| cooldown | 24h | 两次提醒的最小间隔 |
| budget | 3 次 | 最多提醒几次，用尽即静默 |
| expiry | 90 天 | 多久后过期，0 = 不过期 |
| 每轮上限 | 3 条 | `intent_max_per_turn` |

**判定用的是每一行自己的值**，`[proactive]` 配置里的同名项只作为新建时的默认值。这样你可以给某条特定待办设更激进或更保守的策略。

## wake

立即或在下次心跳注入唤醒文本，驱动一轮主动 agent。

## 相关配置

见 [配置参考](../reference/config.md) 的 `[proactive]` 段。
