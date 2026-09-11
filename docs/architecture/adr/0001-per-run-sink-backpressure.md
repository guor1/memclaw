# ADR-0001：per-run 专属背压通道（P0-1 事件丢失修复）

## 状态

已采纳（2026-08-31 落地，v0.1.1 发布）

## 背景

所有事件——包括流式回复的每个 token——通过一个固定容量 256 的 tokio broadcast channel 广播给所有连接。这个设计有两个叠加的缺陷：

**broadcast 容量固定 256，长回复必然溢出。** 流式回复每个 token 一个 `Assistant` delta 事件，一次几百字的回复轻松超过 256 个事件。broadcast 的语义是：任何一个订阅者消费不及时，就会收到 `Lagged(n)` 错误，跳过的 n 个事件**永久不可恢复**——channel 里已经被新事件覆盖了。

**转发任务静默丢帧。** `conn.rs` 的每连接转发任务用 `try_send` 投递到出站队列，队列满时返回 `Err` 而代码忽略了它，既不断连也不告警。

两者叠加的结果：慢客户端（终端渲染慢、网络抖动、或只是 TUI 在重绘）会导致长回复中途截断，且没有任何日志能定位。用户看到的是「模型话说一半就停了」。

## 决策

改为 **per-run 专属有界背压通道**（`RunSink`，`crates/oc-server/src/sink.rs`）。

**run 内联事件走专属通道。** 每个 run 的 `Assistant` delta / `Lifecycle` / `Tool` / `Approval` 事件不再进 broadcast，而是走该 run 专属的有界 channel，经 `RunSink::Conn` 定向回发到**发起这个 run 的那条连接**的出站队列。其他连接不关心这些事件，本来就不该收到。

**慢客户端触发背压而非丢帧。** 通道满时发送方 `.await` 挂起（而非 `try_send` 丢弃），run 的 driver 在 `select!` 里同时等待 cancel token，因此挂起期间可被 abort 打断，**不锁车道**。

**broadcast 只留低频事件。** `Usage`（每 run 一次）和 `Proactive`（cron/intent 推送）保留 broadcast——这两类是真正需要广播给所有连接的，且频率低到不会触发 `Lagged`。

## 后果

**正面：**

- 长回复不再截断。回归测试 `long_reply_not_truncated` 构造 400 个 delta 经真实连接完整送达。
- 客户端断连时 run 立即收敛。`RunSink::closed()` 让静默等待期（建流、等首个 delta、ask_user、审批）也能探测到断连，不必等空闲看门狗兜底（生产默认 120s）。
- 内存占用可控。有界通道 + 背压，不会因为慢客户端导致事件无限堆积。

**代价：**

- 事件路由逻辑变复杂：需要区分「内联事件」（定向回发）与「广播事件」（所有连接），新增事件类型时要判断归属。
- `conn.rs` 的收尾从 `writer_handle.await` 改为 `abort()`——活跃 run 持有的 `out_tx` clone 会让写任务永不退出，`closed()` 也就永不触发。
