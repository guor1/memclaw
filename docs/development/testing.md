# 测试指南与覆盖矩阵

> 2026-09-06 起，真机验证从**手册照着敲**改为**自动化执行**。
> 本文回答两个问题：怎么跑，以及某个行为到底测没测。
>
> 手册模式被弃用的原因不是写得不好，而是三个不可修复的缺陷：会随代码腐化且
> 无人报错；人工执行有静默陷阱（见下方「环境陷阱」第 1 条）；开四个终端跑半天
> 的事实际上不会被反复做——P2 的 12 项当时只跑了 3 项。

## 怎么跑

```sh
cargo test --workspace          # 全部自动化回归（约 60s）
cargo test -p oc-server         # 单 crate
cargo test --test e2e_long_reply  # 单个 e2e 文件
bash scripts/e2e/smoke.sh       # 进程级冒烟（真二进制 + 真 CLI + 真 HTTP）
cargo test -- --ignored         # live lane（需真 API key，默认跳过）
```

CI（[ci.yml](../../.github/workflows/ci.yml)）在每次 push / PR 上跑前两项 +
clippy，Linux 与 Windows 双平台。冒烟在 Linux 上跑。

## 二进制与子命令

`cargo build --release` 只产出**一个**二进制 `target/release/oc`
（[oc-cli/Cargo.toml](../../crates/oc-cli/Cargo.toml) 的 `[[bin]] name = "oc"`）。
所有功能按子命令区分；`oc-tui` 是库 crate，不单独产出二进制。

| 命令 | 作用 |
|---|---|
| `oc`（无子命令） | 连 daemon 进 TUI 对话 |
| `oc serve [--socket]` | 启动 daemon |
| `oc http [--port] [--socket] [--max-conns]` | 启动 OpenAI Responses API 兼容网关 |
| `oc onboard` | 初始化 `~/.oc` 骨架 |
| `oc doctor` | 建库/迁移检查、配置校验 |
| `oc cron` / `oc intent` / `oc memory` | 定时任务 / 话题待办 / 记忆检索 |
| `oc status` / `oc sessions` / `oc debug [--watch]` | 状态、会话列表、诊断快照 |

## 三层结构

| 层 | 位置 | 覆盖 | 代价 |
|---|---|---|---|
| 单测 | `crates/*/src/**` 的 `#[cfg(test)]` | 纯函数、策略判定 | 毫秒 |
| 集成 / e2e | `crates/*/tests/*.rs` | 经真传输的完整链路、真 store、脚本化模型 | 秒 |
| 进程级冒烟 | `scripts/e2e/smoke.sh` | clap 解析、config_loader、paths、真二进制、单实例锁 | ~20s |
| live lane | `#[ignore]` + `OC_LIVE_TEST=1` | 真 provider 的 SSE / 鉴权 / 限流 | 分钟 + 花钱 |

分层依据是**能否确定性断言**，不是快慢。判定标准明确的一律进代码；
只有需要人眼判断质量的才留在 [人工探针清单](manual-probes.md)。
这条界线借鉴 openclaw 的 `qa suite` / `qa manual` 之分。

## 写 e2e 用的 harness

`oc_server::testing`（feature `test-support`）：

```rust
use oc_server::testing::{TestDaemon, test_cfg, SessionConfigExt};

let daemon = TestDaemon::start("我的用例", Arc::new(MockProvider::echo_text("hi"))).await;
let mut client = daemon.client().await;          // 已完成握手
let turn = client.chat_turn("你好").await;        // 收到终态为止
assert_eq!(turn.text(), "hi");
assert!(turn.ended_ok());
```

需要改配置时：

```rust
let daemon = TestDaemon::builder("tag", provider)
    .map_cfg(|c| c.with_queue_cap(1).with_idle_timeout(Duration::from_secs(30)))
    .heartbeat(Duration::from_millis(200))   // 验 cron / 主动性时调小
    .store(oc_store::Store::open_path(db)?)  // 需跨重启持久化时
    .start().await;
```

`tag` 必须在同进程内唯一——Windows 命名管道 `first_pipe_instance(true)` 下
同名二次绑定会失败。

`test_cfg()` 取代了此前散在 18 个测试文件里的 36 处 `SessionConfig` 字面量
（该 struct 有 20 个字段且无 `Default`，每加一个字段要改 20 处，是新测试写不动的主因）。

## 覆盖矩阵

原手册用例 → 现归宿。`✅` = 已有自动化断言。

### P0 稳定核心（原 `P0-运行时测试用例.md`）

| 原用例 | 归宿 |
|---|---|
| TC-1 长回复不截断 | ✅ [e2e_long_reply.rs](../../crates/oc-server/tests/e2e_long_reply.rs) `long_reply_survives_real_transport` + [long_reply.rs](../../crates/oc-server/tests/long_reply.rs) |
| TC-2 背压不丢字 | ✅ 同上 `slow_consumer_gets_backpressure_not_loss`（**注意边界**，见文件头注释） |
| TC-3 abort 打断审批 | ✅ [approval_cancel.rs](../../crates/oc-server/tests/approval_cancel.rs) |
| TC-4 看门狗释放车道 | ✅ [antistuck.rs](../../crates/oc-server/tests/antistuck.rs) `health_scan_aborts_stuck_run` |
| TC-4b 审批放行 | ✅ [toolloop.rs](../../crates/oc-server/tests/toolloop.rs) |
| TC-5 审批 registry 不泄漏 | ✅ `approval_cancel.rs` 断言 `registry.is_empty()` |
| TC-6 大目录 grep 不冻结其它会话 | ✅ [e2e_disconnect.rs](../../crates/oc-server/tests/e2e_disconnect.rs) `slow_session_does_not_block_others` |
| TC-7 断连不锁车道 | ✅ `e2e_disconnect.rs` `disconnect_while_streaming_releases_lane`（**另见下方发现的窗口**） |
| TC-8 多会话事件隔离 | ✅ [multi_session.rs](../../crates/oc-server/tests/multi_session.rs) + `e2e_disconnect.rs` `sessions_do_not_cross_talk_over_transport`（真传输） |

### P1 助手功能

| 原用例 | 归宿 |
|---|---|
| TC-P1-1a~d ask_user | ✅ [ask_user.rs](../../crates/oc-server/tests/ask_user.rs) |
| TC-P1-1e 断连不锁车道 | ✅ 与 TC-7 同源 |
| TC-P1-2a intent CLI 往返 | ✅ `smoke.sh` §3 |
| TC-P1-2b~h intent 触发语义 | ✅ [standing_intent.rs](../../crates/oc-server/tests/standing_intent.rs)（命中注入/无关不注入/cooldown/budget/过期/每轮上限） |
| TC-P1-3a~e,g,h 偏好 supersede | ✅ [memory_write.rs](../../crates/oc-server/tests/memory_write.rs) |
| TC-P1-3f 老库 v1 升级不丢记忆 | ⬜ **未覆盖**（需签入 v1 schema fixture 库，见下方「未覆盖」） |
| TC-P1-4a~g,i dreaming 重写 | ✅ [dreaming_memory_md.rs](../../crates/oc-server/tests/dreaming_memory_md.rs) |
| TC-P1-4h 重写内容无编造 | → [人工探针](manual-probes.md)（质量判断） |
| TC-P1-5a~c,e~g cron 工具 | ✅ [cron_tool.rs](../../crates/oc-server/tests/cron_tool.rs) |
| TC-P1-5d 到点消息真推到客户端 | ✅ [e2e_cron_push.rs](../../crates/oc-server/tests/e2e_cron_push.rs)（含「一次性不重复」回归） |
| TC-P1-6 episodic 产出 | ✅ [episodic_flush.rs](../../crates/oc-server/tests/episodic_flush.rs) |

### oc-http 网关（原 `P2-oc-http真机端到端测试用例.md`）

`cargo test -p oc-http` 有 41 个单测覆盖 adapter 层的纯逻辑（session key 校验、
路由优先级、请求/响应映射）。下表是需要真 daemon 的部分。

网关 e2e 在 [oc-http/tests/gateway.rs](../../crates/oc-http/tests/gateway.rs)：
真 axum（OS 分配端口）+ 真 `ConnPool` + 真 daemon。

| 原用例 | 归宿 |
|---|---|
| TC-H1 裸请求落 main | ✅ `gateway.rs` `bare_request_lands_on_main` + `smoke.sh` §4 |
| TC-H2 session key 路由 + 隔离 | ✅ 单测（`adapter.rs`）+ `gateway.rs` |
| TC-H3 同会话并发排队 | ✅ `gateway.rs` `concurrent_same_session_all_return` |
| TC-H4 队列满 → 报错而非挂住 | ✅ `gateway.rs` `queue_full_returns_error_not_hang` + [queue_full_reject.rs](../../crates/oc-server/tests/queue_full_reject.rs)（server 侧同源） |
| TC-H5 HTTP 与 TUI 抢车道 + TUI 错误显示 | → [人工探针](manual-probes.md)（TUI 渲染） |
| TC-H6 SSE 事件序列与终止 | ✅ `gateway.rs` `sse_stream_emits_deltas_then_completes`（含线格式，**见下方缺陷 2**） |
| TC-H7 断连归还许可 | ✅ `gateway.rs` `aborted_request_returns_permit` |
| TC-H8 max-conns 超限 503 | ✅ `gateway.rs` `over_max_conns_returns_503` |
| TC-H9 previous_response_id 复用会话 | ✅ 单测（`adapter.rs`） |
| TC-H10 非法 session key 被拒 400 | ✅ `gateway.rs` `session_key_validation` + `smoke.sh` §4（均含正反两例，见下方教训） |
| TC-H11 cancel 端点打断 | ✅ `gateway.rs` `cancel_endpoint_aborts_run`（验到车道真释放，不只是 200） |
| TC-H12 真实 OpenAI SDK 打通 | ⬜ **未覆盖**（需 live lane + 真 key，见下方「未覆盖」） |

> **写这批测试时抓到一个真实缺陷**（Windows）：并发请求随机返回 500，
> 报「所有的管道范例都在使用中」（`os error 231` = `ERROR_PIPE_BUSY`）。
> 根因是 `ConnPool` 的 `ClientOptions::open` 单次尝试即失败，而 server 的
> accept 循环一次只备一个管道实例——连上之后才创建下一个，所以并发客户端
> 必然撞上这个窗口。Windows 对 `ERROR_PIPE_BUSY` 的标准处理就是等一下重试
> （本仓 `roundtrip.rs` 早就重试 20 次，生产代码却没有）。
> 已在 [conn_pool.rs](../../crates/oc-http/src/conn_pool.rs) 补上有界重试。
>
> 这个缺陷手册永远抓不到：顺序敲几条 `curl` 不会触发竞态，而手册里
> TC-H3 标注的状态正是「⚠️ 部分通过」。

## 自动化过程中发现的缺陷

**1. SSE 线格式双层 `data:` 前缀 🔴 已修。**
`event_to_sse` 产出的已经是完整 SSE 帧（`data: {json}\n\n`），而 axum 的
`Event::data()` 会再包一层，线上实际发出的是 `data: data: {...}`——
**任何标准 SSE 客户端（含 OpenAI SDK）一个事件都解析不出来**，流式功能等于不可用。

`oc-http` 的 41 个单测全绿也没发现：它们只断言 `event_to_sse` 的返回值，
碰不到 axum 的封帧那一步。手册的 TC-H6（SSE）与 TC-H12（真 SDK）都标着「⬜ 未测」。

已在 [sse.rs](../../crates/oc-http/src/sse.rs) 修复（`split_sse_data` 拆帧后交给
axum 封一次），并由 `gateway.rs` 的 `sse_stream_emits_deltas_then_completes`
逐行断言「单层前缀 + 合法 JSON」钉住。

**2. `ERROR_PIPE_BUSY` 未重试 🟠 已修** —— 见上方网关表格下的说明。

**3. 断连收敛盖不住「等模型」那段窗口 🟠 已修（2026-09-06）。**
断连靠 `emit_inline` 往 sink 发送失败来探测，而 run 在**建流 + 等首个 delta**
期间没有任何事件外发——这段时间客户端断开是**探测不到**的，车道要一直占到
空闲看门狗超时（生产默认 `idle_cloud_secs = 120`，最长 2 分钟）。
`RunSink::closed()` 本就是为这类静默等待期准备的，只是当时仅用在
ask_user / 审批的等待上。已在 [run.rs](../../crates/oc-server/src/run.rs) 的两处
等待（`stream_chat` 建流、`stream.next()` 取 delta）叠上 `sink.closed()`。
`e2e_disconnect.rs` 的两条用例分别钉住两个窗口：
`disconnect_while_streaming_releases_lane`（流式中断连，靠 send 失败探测）与
`disconnect_before_first_delta_converges_fast`（静默期断连，靠 `closed()`）。
后者的 `idle_timeout` 刻意设成 120s，确保断言到的收敛不可能来自看门狗兜底。

以下一处**未修**（跨 crate，超出本次范围），已钉成用例：

**4. `usage.input_tokens` 并发下不可信。**
`Event::Usage` 只有 `session` 没有 `run_id`，同会话 N 个并发 run 无法归属；
且它走全局广播、比 `Lifecycle::End` 晚到时会被 `accumulate_response` 丢弃。
详见 [OpenAI-Responses-API-方案 §8](../reference/protocol.md)。
修它要动 `oc-proto`，跨 crate，留待 P2。

## 未覆盖（原手册已删，用例规格记在此）

这两条自动化尚未做到，删手册时把规格留在这里，别让它们随文件一起消失。

**TC-P1-3f 老库 v1 升级不丢记忆。**
`memory` 表的 `pref_key` 列是迁移 v2 加的（`ALTER TABLE ADD COLUMN`）。
要验的是：拿一个 v1 schema 的库启动，迁移后原有记忆条目一条不少、
且新的 supersede 逻辑能正常工作。
做法：造一个 v1 库签入 `crates/oc-store/tests/fixtures/`，
测试里 `Store::open_path` 它、跑迁移、断言条目数与内容。
现状：`memory_write.rs` 覆盖了 supersede 语义，但都是新建库，迁移路径没测。

**TC-H12 真实 OpenAI SDK 打通。**
用 Python `openai` SDK 指向本网关跑一次对话，确认真实客户端能解析我们的
响应与 SSE 流。价值很高——上面「缺陷 1」（SSE 双层 `data:` 前缀）
正是这条从未跑过才漏到今天的。
做法：`#[ignore]` 用例 + `OC_LIVE_TEST=1` 门控，或独立脚本。
`gateway.rs` 的 SSE 线格式断言已经补上了一部分保护，但真 SDK 的兼容面更广。

## 环境陷阱

三条都是踩过的坑，且**失败方式都是静默的**——这类问题正是自动化要消灭的对象。

**1. cmd.exe / PowerShell 里 `&` 不是后台符**，是命令分隔符。并发用例会变成
顺序执行、全部返回 200，看着像通过，实际什么都没测到。`scripts/` 下的
bash 脚本必须用 Git Bash 跑（`C:\Program Files\Git\bin\bash.exe`）；
`smoke.sh` 开头有 `$BASH_VERSION` 自检。

**2. `OC_HOME` 在 Windows 上必须是 Windows 路径格式**（`C:\...` 或 `C:/...`）。
写成 MSYS 风格 `/c/...` 会被 Rust 当成当前盘根下的 `\c\...`，数据落到垃圾路径，
而 `oc doctor` 原样回显、库照样建得出来，**完全看不出错**。
`smoke.sh` 用 `cygpath -w` 转换。

**3. `OC_HOME` 不隔离连接**。Windows 的管道名是全局常量 `\\.\pipe\oc-daemon`
（`TransportKind::platform_default` 直接忽略 `oc_home`），所以改了 `OC_HOME` 再跑
`oc sessions`，返回的仍是当前占着管道的那个 daemon 的数据。
为此新增了 **`OC_SOCKET`** 环境变量：`oc serve` 与所有 CLI/TUI 客户端都尊重它，
测试因此能与开发机上跑着的 daemon 隔开。`oc serve --socket` 也可显式指定
（对称于既有的 `oc http --socket`）。

## 一条教训：断言要防自己假通过

写 `smoke.sh` 的 TC-H10 时，最初用 `x-openclaw-session-key: bad key!!` 断言
返回 400。它**通过了**，但测的完全是另一件事——`validate_session_key` 只拒
空串/超长/控制符/保留前缀，带空格叹号的 key 其实合法；那个 400 来自请求体里
中文经 shell 传给 curl 时坏掉的 JSON 解析。

现在改用保留前缀 `cron:smoke`（真非法 → 400），并**加一条反向对照**：
含空格的 `smoke lane 1` 应返回 200。两条一起才能证明 400 来自前缀判定，
而不是「请求随便就会 400」。

同理，`e2e_long_reply.rs` 的文件头如实记录了它**没能**覆盖到的路径
（`try_send` 队列满丢帧，因内核缓冲吸收 16MB 而无法从客户端侧触发）。
宁可写明边界，也不要让人误以为某条路径已有保护。

## 相关

- [人工探针清单](manual-probes.md) —— 只剩需要人眼判断的少数几条
- [看板](../../BOARD.md) —— 未完成的工作（含 TEST-1 / TEST-2 / TEST-3 三条测试缺口）
- [CHANGELOG](../../CHANGELOG.md) —— 已发生的变更
