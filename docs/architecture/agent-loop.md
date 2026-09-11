# Agent 循环与工具

Agent 循环由 `oc-core` 的纯状态机驱动，`oc-server` 负责执行副作用。这个分工让循环逻辑可以确定性单测，而 IO、并发、时钟都隔离在外围。

## 车道模型

每个 session 一条车道，同一会话同一时刻只有一个 run 在执行。新消息在车道占用期间排队。

`cron`、`dreaming`、`Lane2` 各自起隔离子会话，有独立车道，不占用 `main`——这是「夜间巩固不影响你正在打字」的前提。

## 一轮 run 的生命周期

```
消息入队 → 占车道 → Lane1 记忆检索（并行，见 memory.md）
  ↓
core::prompt::render 组装 prompt（确定性排序）
  ↓
oc-llm 建流 → 逐 delta 回发（经 RunSink，见 ADR-0001）
  ↓
模型要调工具？
  ├─ 是 → 工具策略判定 → 需审批则等回执 → 执行 → 结果净化 → 回喂模型（回到建流）
  └─ 否 → 落库 transcript → 释放车道
```

每一步之后 server 都会调 `core::detect_loop` 检查是否在打转。

## 工具集

`oc-tools` 提供七个工具，每个工具声明自己的 `ToolSpec`（进 prompt 的参数 schema）和 `ToolPolicy`（是否需审批 / 超时 / 能否转后台）。

| 工具 | 说明 | 策略要点 |
|---|---|---|
| `exec` | 跑一次性命令或脚本 | **审批门**，危险类默认 deny；工具级超时；结果净化 |
| `process` | 长命令转后台、可 kill、查状态 | 进 background task 台账，返回 handle |
| `file` | 读 / 写 / 列 / edit / append | 路径白名单；写操作进审计台账 |
| `web_fetch` | 抓 URL 转 markdown | 出站请求；结果标记 `Untrusted` |
| `web_search` | 搜索 | 同上，结果 `Untrusted` |
| `ask_user` | 主动提问，阻塞 run 等答复 | 走事件流推到 client |
| `cron` | 定时任务增删查（`add`/`delay`/`list`/`rm`） | 见 [proactive.md](proactive.md) |

`web_fetch` 和 `web_search` 的结果标 `Untrusted` 不是形式主义——它让 dreaming 的门 2 能结构性拒绝把网页内容巩固进长期记忆。

## 审批门

```rust
pub enum ApprovalDecision { Allow, AllowOnce, Deny, AllowAndRemember(Rule) }
pub fn classify_command(cmd: &str, rules: &PolicySet) -> RiskClass; // Safe | NeedsApproval | Blocked
```

危险命令（`rm -rf`、写系统目录、`curl | sh` 等）判为 `NeedsApproval`，server 发审批请求给 client，回执驱动状态机继续。分类是纯函数，可单测，规则集可配。

审批等待套了 `select!` + `cancel.cancelled()`，所以 abort 能立即打断等待中的审批而不是干等到超时。registry entry 用 RAII 守卫清理，覆盖正常回执 / 超时 / abort / pump 被 abort 所有退出路径。

审批模式由 `[tools.approval] mode` 配置：`prompt`（默认）/ `allow` / `deny`。**无人值守场景**（cron、`oc http`）要注意 `timeout_secs` 不能设 0，否则该轮会永久占用车道直到卡死兜底才释放。

## 结果净化

工具输出在回喂模型前会截断超长内容、剥控制字符、限制注入体积。这同时防上下文爆和防注入。

## 防卡死：七种死法

「慢 ≠ 卡」是核心判据。abort 阈值要同时满足「距上次进展 ≥ `abort_min_secs`（默认 300s）」和「≥ 3× 警告阈值」才触发，未达阈值的慢 run 保持 `LongRunning` 状态不误杀。

判据是**多久没有进展**（`last_delta_at`），不是 run 总墙钟时长——一个连续输出的 157 秒 run 不会被判卡死。

| 死法 | 机制 | 落点 |
|---|---|---|
| 模型不吐 token | 空闲看门狗（per-delta timeout） | server 包 provider 流 |
| run 跑太久 | run 超时 + abort timer | server run task |
| 工具卡住 | 工具级超时 + `process` 转后台 | tools policy + server |
| 模型打转 | loop detection | core 判定 + server 调用 |
| 上下文爆 | 压缩 + overflow 重试 + 结果剪枝 | core compaction + server |
| 车道被占死 | 卡死诊断 + abort-drain 释放车道 | core queue + 心跳扫描 |
| 用户要打断 | `chat.abort`（先 drain 排队轮再中止活跃 run） | proto + server queue |
| 崩溃 / 重启 | transcript 落库续接 + 台账补 push | store + ledger |

阈值默认值见 [配置参考](../reference/config.md) 的 `[watchdog]` 段。

## panic 隔离

release profile 用 `panic = "unwind"`（不是 abort），run 主体套 `catch_unwind`：单个 run 或单个工具 panic 会归一成 `RunOutcome::Panicked` → 发 `lifecycle(error)` → 释放车道，**进程继续存活**。

## 上下文压缩

上下文接近窗口上限时触发压缩：调摘要模型把旧历史压成摘要，保留近期原文。压缩**之前**会先把要被压掉的那批历史 flush 成 episodic 记忆候选（见 [memory.md](memory.md)）——那是它们进入长期记忆的最后机会。

Token 估算当前用「字符数 / 4」近似，`tiktoken` 作为可选 feature 预留但未启用。
