# 06 — zeroclaw 流式传输/输出机制调研

> 调研日期：2026-08-31。目标项目：`C:\dev\workspace\zeroclaw`（成熟生产级 Rust agent 项目）。
> 动机：为 [05-下一阶段计划.md](05-下一阶段计划.md) P0-1「事件丢失（截断根因）」寻找参考解法。
> 结论用于决策：MyClaw 采用「路 B — per-run 背压流」重构 run 输出通道。

## 结论先行

zeroclaw 在「模型 token → 客户端」这条关键链路上**完全不用 `broadcast`**，而是**每个 turn 一条专属的有界 `mpsc` 通道 + 全程 `.send().await` 背压**。

`broadcast` 只用于与 token 流无关的旁路（日志广播、cron/心跳全局事件、canvas 帧、SSE 观测事件），且在那些地方**显式选择「丢弃」语义**（吞掉 `Lagged`）。

也就是说：MyClaw P0-1 担心的「共享广播通道容量溢出导致 token 丢失」，zeroclaw 从架构上直接规避了——**token 根本不走 broadcast**。

## 核心设计：两类通道的职责二分

| 用途 | 通道原语 | 语义 | zeroclaw 的例子 |
|---|---|---|---|
| **会话 token 流**（不可丢） | 每 turn 一条**有界 mpsc** | `.send().await` **背压** | provider → turn → 传输，全链路一对一 |
| 遥测 / 通知（可丢） | `broadcast` | 显式吞掉 `Lagged` | 日志、cron/心跳全局事件、SSE `/api/events`、canvas 帧 |

这个二分本身就是最值得借鉴的设计：**可丢的旁路遥测走 broadcast；不可丢的会话 token 走 mpsc 背压。**

## 1. 传输层：支持哪些协议、入口在哪

| 传输 | 入口文件 | 说明 |
|---|---|---|
| WebSocket（浏览器/仪表盘 chat） | `crates/zeroclaw-gateway/src/ws.rs` | axum `WebSocketUpgrade`，子协议 `zeroclaw.v1`，路由 `GET /ws/chat` |
| HTTP SSE（可观测性事件流） | `crates/zeroclaw-gateway/src/sse.rs` | axum `Sse`，路由 `GET /api/events`；**只传观测事件，不传 token** |
| Canvas WebSocket | `crates/zeroclaw-gateway/src/canvas.rs` + `crates/zeroclaw-tools/src/canvas.rs` | `/ws/canvas/:id` |
| ACP（JSON-RPC 2.0 over stdio） | `crates/zeroclaw-channels/src/orchestrator/acp_server.rs` | 首行注释即 "JSON-RPC 2.0 over stdio" |
| 本地 IPC：Unix socket / Windows 命名管道 | `crates/zeroclaw-runtime/src/rpc/local.rs` | `#[cfg(unix)]` 用 `UnixListener`；`#[cfg(windows)]` 用 `NamedPipeServer`（`\\.\pipe\zeroclaw-<hash>`） |
| WSS（远程 TUI ↔ daemon，TLS WebSocket） | `crates/zeroclaw-runtime/src/rpc/wss.rs` | `TcpListener` + `tokio_rustls` + `tokio_tungstenite` |

传输抽象在 `crates/zeroclaw-runtime/src/rpc/transport.rs`，一个极简 trait：

```rust
#[async_trait]
pub trait RpcTransport: Send + 'static {
    fn writer(&self) -> mpsc::Sender<String>;       // 出站：拿到写通道
    async fn next_frame(&mut self) -> Option<String>; // 入站：逐帧读
    fn peer_label(&self) -> String;
}
```

`LocalTransport`（unix/pipe）和 `WssTransport` 都实现它，dispatcher 完全不关心底层是 socket 还是 WebSocket。**JSON-RPC dispatch 与传输解耦**——新增传输（vsock 等）成本极低，不碰 dispatch/session 逻辑。

## 2. 流式输出机制：token 一路怎么传

纯 **per-request 拉取 + 有界 mpsc 转发** 链路，全程无 broadcast：

- **第 1 段 — provider 出流**：`crates/zeroclaw-api/src/model_provider.rs` 的 `stream_chat` 返回 `BoxStream<'static, StreamResult<StreamEvent>>`。`StreamEvent` 枚举：`TextDelta / ToolCall / PreExecutedToolCall / Usage / Final`。每次请求独立的惰性流，不是广播。
- **第 2 段 — 消费流并转发**：`crates/zeroclaw-runtime/src/agent/turn/stream_consume.rs` 的 `consume_provider_streaming_response`：`provider_stream.next().await` 逐块拉取，每个可见 delta 通过 `.send().await` 转发（**不是 `try_send`**）——消费慢则挂起，即背压。
- **第 3 段 — turn 通道**：两条并行有界 mpsc：`event_tx: mpsc::Sender<TurnEvent>`（结构化事件：`Chunk/Thinking/ToolCall/ToolResult/Plan/Usage/...`，定义于 `crates/zeroclaw-api/src/agent.rs`）与 `on_delta: mpsc::Sender<DraftEvent>`（草稿富文本流）。
- **第 4 段 — 各传输序列化给客户端**：
  - WebSocket：`ws.rs` 建 `mpsc::channel::<TurnEvent>(64)`，`recv()` 后转 JSON 帧。
  - ACP：`acp_server.rs` 建 `mpsc::channel::<TurnEvent>(100)`，经 `notification_for_turn_event` 转 JSON-RPC `session/update` 通知。
  - daemon RPC（TUI）：`rpc/turn.rs` 建 `mpsc::channel::<TurnEvent>(64)`，`execute_turn` 用回调把每事件经 `notification_for_turn_event` → `rpc.send_raw(n).await` 写到传输 writer 通道。

**站队结论：每个请求/每个 run 一条专属流。** 全链路一对一，无任何 fan-out。

## 3. 背压 vs 丢弃

- **token 链路：全程背压**（bounded mpsc + `.send().await`）。provider→turn→传输 writer（`local.rs`/`wss.rs` writer 均为 `mpsc::channel::<String>(64)`）逐级传导，任一环慢则 `provider_stream.next()` 不被 poll → 拉取变慢。**无覆盖、无丢弃、无 `Lagged`。**
- 代价：一个卡住的慢客户端拖慢它自己那一路的 provider 拉取；但因 per-run 独立，不影响别的会话。
- **唯一用 `try_send`（丢弃）处是 steering（用户中途插话）**：`ws.rs` `steering_tx.try_send(content)`，队列满回 `STEERING_QUEUE_FULL`——有意为之，不让插话阻塞主流程。
- **broadcast 的三处（均非 token 流、均选丢弃）**：日志广播 `zeroclaw-log/src/broadcast.rs`（cap 65536）；全局事件总线 `zeroclaw-gateway/src/lib.rs`（`broadcast::channel::<Value>(256)`）；canvas 帧 `zeroclaw-tools/src/canvas.rs`。SSE 消费全局总线时显式 `Err(_) => None // Skip lagged messages`。

## 4. SSE / chunked streaming

- HTTP SSE 只有一条 `GET /api/events`，订阅**全局 broadcast 总线** `state.event_tx.subscribe()`，用 `BroadcastStream` 包成 axum `Sse`。**传观测事件（llm_request/tool_call/agent_end），不是逐 token 输出。**
- 真正逐 token「chunked streaming」走 WebSocket（`type:"chunk"` 帧）或 ACP/RPC 的 `session/update` 通知。
- token 流严格 per-turn 绑定；SSE `/api/events` 是共享广播、非 per-request——但只承载全局遥测，共享合理。
- 注：配对凭证（QR/pair code）是「broadcast-only、delivery-once」，迟连/落后的客户端**故意无法恢复**——安全边界，非 bug。

## 5. 断线 / 重连 / 重同步

zeroclaw **没有** last-event-id 式的 token 级事件重放，采用**持久化会话状态 + 重连后重发快照**：

- **无 token 级重放**：token 只在活跃 turn 的 mpsc 里存在，断线即丢当前 turn 增量（仅保留取消时已产出的 partial text）。
- **会话级恢复**：WebSocket 重连从 `session_backend.load()` 加载历史，发 `session_start{resumed, message_count}`，`seed_history_with_event` 重灌历史；Plan/TodoWrite 经 `plan_replay_notification` + `persist_plan_if_any` 在重连/`session/resume` 时重推整份 plan；usage/cost 落 ACP session store。
- **EventBuffer（SSE 历史）**：`sse.rs` 的 `VecDeque` 环形缓冲，`GET /api/events/history` 拉最近观测事件（不含凭证类）。仅 observability 的重同步，非 token 流。
- **心跳/半开检测**：WSS 20s idle→Ping、10s 无响应→判死；WebSocket 可配 ping interval。断线经 `CancellationToken` 取消 turn 并持久化已产出 partial text（`stream_consume.rs` 的 `StreamCancelledAfterOutput`）。

## 对 MyClaw 的借鉴判断

**强烈可借鉴：**

1. **token 流坚决不用 broadcast，用 per-run 有界 mpsc + `.send().await` 背压**——从根上消除「广播溢出丢事件」。慢消费者只拖慢自己那一路。
2. **传输 trait 抽象**（writer + next_frame + peer_label）让多传输共用同一套 dispatch，新增传输成本极低——正对 MyClaw「以后加 HTTP/socket 不返工」的目标。
3. **两类通道职责二分**：可丢遥测走 broadcast（吞 Lagged）；不可丢 token 走 mpsc 背压。MyClaw 现在的错在于把两类混在一条 256 的 broadcast 里（proactive 可丢 + assistant delta 不可丢）。
4. **恢复策略「持久化会话 + 重连重发快照」而非 token 级 replay**——实现简单，对 MyClaw 够用。

**取舍提醒：**

- 它**没有**精确 token 续传；当前 turn 断线未落盘的增量会丢。若需「断线无缝续上正在生成的那句」，得自己加事件序号 + 重放缓冲。MyClaw 场景不需要。
- SSE 在 zeroclaw 是观测通道（共享广播、可丢），不是 token 流。若 MyClaw 未来想用 SSE 传 token，需做成 per-request 的 SSE（每请求一条 `Sse` 绑一条 mpsc）。

## 关键文件清单（便于深读）

- `crates/zeroclaw-api/src/model_provider.rs` — StreamEvent / BoxStream 定义
- `crates/zeroclaw-api/src/agent.rs` — TurnEvent 定义
- `crates/zeroclaw-runtime/src/agent/turn/stream_consume.rs` — 消费 provider 流 + 背压转发（核心）
- `crates/zeroclaw-runtime/src/agent/turn/events.rs` — StreamDelta / emit 辅助
- `crates/zeroclaw-runtime/src/rpc/transport.rs` — 传输 trait
- `crates/zeroclaw-runtime/src/rpc/local.rs` — unix socket / 命名管道
- `crates/zeroclaw-runtime/src/rpc/wss.rs` — TLS WebSocket + 心跳
- `crates/zeroclaw-runtime/src/rpc/turn.rs` — execute_turn，mpsc(64)
- `crates/zeroclaw-runtime/src/rpc/dispatch.rs` — notification_for_turn_event、plan replay、send_raw
- `crates/zeroclaw-gateway/src/ws.rs` — WebSocket chat，mpsc(64) + steering try_send
- `crates/zeroclaw-gateway/src/sse.rs` — SSE + broadcast + skip lagged + EventBuffer
- `crates/zeroclaw-channels/src/orchestrator/acp_server.rs` — ACP over stdio，mpsc(100)
- `crates/zeroclaw-log/src/broadcast.rs`、`crates/zeroclaw-gateway/src/lib.rs`、`crates/zeroclaw-tools/src/canvas.rs` — broadcast 的三处用途（均可丢旁路）
