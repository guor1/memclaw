# 可观测性设计：结构化日志 + 诊断工具

> 状态：设计稿，待实现。触发背景：出现「消息不回复 / 回复截断 / /reset 触发异常回复」等间歇性问题，
> 逐文件人肉排查成本过高，根因是**缺少 run 级可观测性**。目标是让下一次诡异现象能被日志/诊断直接定位，
> 而不是靠读代码猜。

## 0. 问题性质（为什么需要这个）

已观察到的现象都是**时序/竞态**类，静态读码无法定位：

- 发消息后完全无回复（run 是否起步？模型是否被调用？错误是否被吞？无从判断）
- 回复到一半中断（空闲看门狗？流提前结束？`finish_reason=length`？无法区分）
- `/reset` 后带出一段助手回复（疑似排队轮的迟到响应 / 事件乱序）

当前代码的可观测性盲区：

| 位置 | 盲区 |
|---|---|
| `session.rs:148` `reply.send(run_id)` 在所有 `.await` 之前 | run_id 已返回但 run 未起步的全过程无日志 |
| `session.rs:199` `compact_session().await` 串行占用车道 | compact 阻塞期间新消息静默排队，无记录 |
| `run.rs drive_inner` | 无 run_id/session span；delta、finish_reason、watchdog 无上下文 |
| `run.rs` 事件 `let _ = events.send(...)` | 发了什么事件、是否被 client 过滤丢弃，无记录 |
| daemon 日志打在 `oc serve` 进程 | 用户看的是 TUI 进程，两终端隔离，日志实际不可见 |

---

## A. 结构化日志

### A.1 目标
- 每个 run 有唯一关联 id 贯穿全链（`run_id` + `session_id`），可 grep 出完整时间线。
- 关键节点都有埋点：submit 受理、run 起步、模型调用起止+延迟、每类 delta 计数、finish_reason、工具调用、watchdog、compact 起止、事件广播。
- 落**文件**（daemon 独立进程），可 tail；分级；按天/大小滚动。
- 生产默认 `info`，排障用 `RUST_LOG=oc=debug` 一键放开。

### A.2 span 结构（tracing）

```
run{run_id, session_id}                     ← session actor 起 run 时进入
  ├─ submit_accepted (event)                 提交受理，队列状态
  ├─ history_loaded (event) entries=, tokens=
  ├─ model_turn{turn_idx}                     每一轮模型调用一个 span
  │    ├─ request_built (event) msg_count=, est_tokens=, compacted=
  │    ├─ stream_open (event) latency_ms=     首字节/建流耗时
  │    ├─ delta_summary (event) text_deltas=, reasoning_deltas=, tool_deltas=
  │    ├─ usage (event) input=, output=
  │    └─ turn_done (event) finish_reason=, elapsed_ms=
  ├─ tool_exec{name, call_id}                 工具执行一个 span
  └─ run_done (event) outcome=, total_ms=, tool_rounds=
```

实现要点：
- 用 `tracing::info_span!("run", %run_id, %session_id)`，在 `start_run` 生成、贯穿 `drive`。
  因为 `drive` 在 `tokio::spawn` 里跑，用 `.instrument(span)`（`tracing::Instrument`）附加，
  而非 `entered()`（跨 await 不安全）。
- `run_model_turn` 内 `model_turn` 子 span，记录建流延迟（`Instant` 差）与 delta 分类计数。
- **关键**：把 `session.rs` 里 `reply.send` 之后、`start_run` 之前的每个 await（append_entry / load_history / lane1_bootstrap）都加 event 日志，
  这样「run_id 已返回但迟迟不起步」能立刻看出卡在哪一步。

### A.3 必加埋点清单（明天按此逐条加）

- `session.rs`
  - `Submit`：受理时 `info!(%run_id, queue_state=?, "submit accepted")`
  - `Submit`：`load_history` 前后各一条 `debug!`（起止 + 耗时 + 条数）
  - `Compact`：`info!("compact start")` / `info!(elapsed_ms, "compact done/failed")`，**尤其**记录 compact 期间车道被占用
  - actor loop 顶部：`trace!(cmd=?, "actor recv")`（能看出消息到没到 actor）
- `run.rs`
  - `drive` 入口/出口：`info!(outcome, total_ms)`
  - `run_model_turn`：建流耗时、finish_reason、四类 delta 计数
  - watchdog 触发点（`run.rs:333` 空闲看门狗、`run.rs:69` run 超时）：已有 warn，补 `elapsed_ms` + 当前累计文本长度，判断「截断」根因
  - 每次 `events.send`：`trace!(event=?, "emit")`（可选，debug 级）
- `openai.rs`
  - 非 2xx 已记录（`openai.rs:137`），保留
  - 建流成功后首个 chunk 到达：`debug!(latency_ms, "first chunk")`
  - `[DONE]` / 流自然结束：`debug!("stream end", reason)`
- `summarize.rs`
  - 起止 + 输入消息数 + 超时命中（`summarize.rs:79` 已有 warn，补 `input_msgs=`, `elapsed_ms=`）

### A.4 日志落盘

`oc serve` 启动处（`main.rs:108 run_serve`）改造：

```rust
// 依赖：tracing-appender
let home = paths::oc_home()?;
let log_dir = home.join("logs");
std::fs::create_dir_all(&log_dir)?;
let file_appender = tracing_appender::rolling::daily(&log_dir, "oc.log");
let (nb, _guard) = tracing_appender::non_blocking(file_appender);
// _guard 必须存活到进程结束，否则日志丢失 → 提到 run_serve 顶层持有

tracing_subscriber::fmt()
    .with_writer(nb)
    .with_env_filter(
        EnvFilter::try_from_default_env().unwrap_or_else(|_| "oc=info,oc_server=info".into())
    )
    .with_target(true)
    .with_ansi(false)      // 文件不要颜色码
    .with_span_events(FmtSpan::CLOSE)  // span 关闭时打耗时
    .init();
```

- 位置：`~/.oc/logs/oc.log.YYYY-MM-DD`（`paths::oc_home()`）。
- 保留策略：`tracing-appender` 的 rolling 不自动清理旧文件，需补一个启动时清理 >N 天的小函数（可选，明天次要）。
- 同时保留 stderr 输出（可选，用 `.with_writer(nb.and(std::io::stderr))` 或分层 Layer），方便前台跑 `oc serve` 时直接看。

### A.5 依赖
- `tracing-appender`（新增）
- 已有 `tracing` / `tracing-subscriber`，确认 `tracing-subscriber` 开了 `env-filter` feature，`fmt` 需 span-events。

---

## B. 诊断 / 监控工具

日志是「事后回溯」，诊断工具是「实时快照」——间歇性问题需要在**现场**看内部状态。

### B.1 服务端：run 级指标收集

在 `ServerState` 或 `SessionRegistry` 侧维护每会话/每 run 的运行时快照。当前
`state.rs` 只存了 `usage`（last_input_tokens），信息太少。新增一个轻量 registry：

```rust
// 新文件 crates/oc-server/src/diag.rs
pub struct RunSnapshot {
    pub run_id: String,
    pub session_id: String,
    pub phase: RunPhase,          // Queued/AwaitingModel/Streaming/ToolExec/Done
    pub started_at: i64,          // unix ms
    pub last_delta_at: i64,       // 最近一次收到 delta 的时间 → 判「卡在哪」
    pub tool_rounds: usize,
    pub last_input_tokens: Option<u32>,
    pub last_error: Option<String>,
}

pub struct SessionDiag {
    pub session_id: String,
    pub queue_depth: usize,       // 排队轮数
    pub active: Option<RunSnapshot>,
    pub lane_busy_since: Option<i64>,  // 车道被占用起始（compact/长 run 一眼看出）
    pub total_runs: u64,
    pub last_finish_reason: Option<String>,
}
```

- session actor 在状态迁移点更新自己的 `SessionDiag`（放进 `Arc<DashMap<SessionId, SessionDiag>>`，
  与 `usage` 并列挂在 `ServerState`）。
- 更新点复用 A.3 的埋点位置——每处埋日志的地方顺手写一次快照，成本极低。
- `lane_busy_since` 是关键：能直接暴露「compact 阻塞车道」「run 卡死不释放」这类本次遇到的问题。

### B.2 新协议方法

`oc-proto/src/method.rs` 加：

```rust
Method::Diagnostics                      // 请求整机诊断快照
MethodOk::Diagnostics(DiagnosticsSnapshot)

pub struct DiagnosticsSnapshot {
    pub uptime_secs: u64,
    pub sessions: Vec<SessionDiag>,
    pub store_writer_alive: bool,        // 写线程健康（ping 一次）
    pub event_subscribers: usize,        // 当前订阅者数
    pub proto_version: u32,
}
```

`PROTO_VERSION` 升到 3（greenfield 无兼容包袱，见 memory）。

### B.3 CLI 工具

`oc debug`（新子命令，类比现有 `oc status`）：

- `oc debug` → 打印整机诊断快照（会话表、活跃 run、队列深度、车道占用时长、写线程状态）。
- `oc debug --watch` → 每秒刷新（复用 `cli_client` 的连接），看「卡住」时状态如何演变——
  这对间歇性问题最有用。
- 输出示例：
  ```
  uptime 00:12:34   writer OK   subs 1
  session  queue  phase        run_id     age    last_delta  tools  err
  main     0      Streaming    01932...   4.1s   3.9s        0      -
  110      1      AwaitingMdl  01931...   61s    -           0      -   ← 卡 61s 无 delta
  ```
  一眼看出「110 会话卡在等模型 61 秒无 delta」——正是本次要抓的现象。

### B.4 TUI 增强（次要，锦上添花）

- 状态栏已有 `ctx X/Y`，补一个调试开关（如 `/debug`）在侧栏显示当前 run 的 phase + age + last_delta。
- 或者简单点：`/debug` 命令直接把 `Diagnostics` 快照当消息打印到会话流，不改布局。

---

## C. 落地顺序（明天）

按「最快能抓到本次 bug」排序：

1. **A.4 日志落盘 + A.3 session/run 关键埋点**（最高优先——先让 daemon 日志可见、run 生命周期可回溯）。
   做完这步，重现一次「不回复」就能直接从 `~/.oc/logs/oc.log` 看出卡在哪。
2. **B.1 + B.2 + B.3 `oc debug`**（实时快照，尤其 `--watch` 和 `lane_busy_since`）。
3. A.2 完整 span 树细化（turn 级计时、delta 分类）。
4. B.4 TUI 调试面板（可选）。

## D. 顺带能定位的本次疑点

这套东西上线后，可直接验证以下假设（现在只能猜）：
- 「不回复」= run 未起步（卡在 `load_history`/写线程）还是模型无响应？→ 看 run span 是否出现、stream_open 是否记录。
- 「回复截断」= 空闲看门狗（`run.rs:333`）还是 `finish_reason`？→ 看 `turn_done` 的 finish_reason 与 watchdog warn。
- 「/reset 带出回复」= 排队轮迟到 → 看 `queue_depth` 与 run 时间线是否有跨 /reset 的迟到 `run_done`。
- 「compact 卡住整条车道」→ 看 `lane_busy_since`。
