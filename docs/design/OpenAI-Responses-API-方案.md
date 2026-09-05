---
summary: "oc-http：OpenAI Responses API 兼容层的落地方案与边界"
read_when:
  - 要对接 OpenAI SDK / LangChain 等客户端
  - 要改 oc-http 的协议映射或 SSE 事件
  - 想知道某个 OpenAI 参数支持到什么程度
---

# OpenAI Responses API 兼容层

`oc http` 在 daemon 前面加一层 HTTP 适配器，把 `POST /v1/responses` 翻译成内部 NDJSON 协议。
目标不是完整复刻 OpenAI，而是让主流文本对话客户端能直接连上来——覆盖约 40–50% 的接口面，
其余部分显式拒绝而不是静默降级。

用法见 [README 的「OpenAI API 兼容」](../../README.md#openai-api-兼容http-网关)。

---

## 1. 架构

HTTP 层是**独立进程**，经 socket/pipe 连到已在跑的 daemon：

```
OpenAI SDK ──HTTP/SSE──▶ oc http (oc-http) ──NDJSON──▶ oc serve (oc-server)
```

这样切分的理由：协议适配的依赖（axum/tower）不进核心；HTTP 侧崩溃或升级不影响会话；
daemon 的权限模型、审批门、记忆机制原样复用，无需在两处维护。

代价是需要起两个进程。集成为 `oc serve --http` 是可选的后续项，当前不做。

### 模块划分

```
crates/oc-http/src/
├─ types.rs      OpenAI 请求/响应/SSE 事件的 serde 类型
├─ adapter.rs    协议映射：会话解析、input 归一化、前缀构建、参数校验
├─ conn_pool.rs  到 daemon 的连接池（NDJSON codec 任务）
├─ sse.rs        Event → SSE 事件的状态机
├─ server.rs     axum 路由与 handler
└─ error.rs      错误 → HTTP 状态码
```

---

## 2. 三个关键映射

OpenAI 的有状态语义与 memclaw 的会话模型并不天然对齐。以下三处是方案成立的关键，
思路参考 OpenClaw 的 `openresponses-http-api`——它证明这些**不需要**改动核心架构。

### 2.1 `previous_response_id` → 复用会话

OpenAI 的原意是「引用上一个响应的 output 作为历史」。若照字面实现，需要把每个
response 的 OutputItem 数组独立存起来，与 memclaw 的线性 transcript 冲突。

实际只需**复用那次请求所在的会话**：记住 `response_id → SessionId`，后续请求落回同一个
session actor，历史由 daemon 自己拼。映射放在内存（`DashMap`），重启后失效——此时降级为
默认会话而不是报错，因为「继续对话」的意图用默认会话仍能满足，报 404 反而更糟。降级时
`user` 派生仍然生效：陈旧的 response id 不该把调用方的身份作用域一起丢掉。

优先级：`x-openclaw-session-key` 头 → `previous_response_id` → `user` 派生 → `main`。

```rust
// adapter.rs
pub fn resolve_session(
    req: &CreateResponseReq,
    session_key: Option<SessionId>,   // 来自 x-openclaw-session-key
    sessions: &ResponseSessions,
) -> SessionId {
    if let Some(sid) = session_key { return sid; }   // 显式路由优先于任何推断
    if let Some(prev) = &req.previous_response_id {
        if let Some(sid) = sessions.get(prev) { return sid.clone(); }
    }
    if let Some(user) = &req.user {
        // 同一个 user 稳定映射到同一会话，跨请求共享上下文（与 main 隔离）
        return SessionId::new(format!("http-user-{:x}", hash(user)));
    }
    SessionId::main()   // 默认与 TUI 同一条对话
}
```

`x-openclaw-session-key` 沿用 OpenClaw gateway 的头名，照它写的客户端不用改。保留前缀
`subagent:` / `cron:` / `dreaming:` / `acp:` 一律 400；键还要求非空、≤128 字符、无控制字符。
与其余解析路径不同，**非法 key 是硬错误**：调用方指名了会话，静默跑到别处比 400 更糟。

#### 为什么默认是 `main` 而不是 OpenClaw 的「每请求独立」

1. **无状态默认在本项目是泄漏。** `registry.rs` 明确「无空闲淘汰」，而 session actor 一起来就
   `ensure_session` 落库。每请求一个新 session id 会永久留下一个常驻 actor + 一行 `session`
   记录，无界增长。默认走 `main` 让最常见路径零新增。
2. **memclaw 是单用户常驻伴侣，HTTP 网关只是另一个 client。** `oc http` 只绑 loopback 且无鉴权，
   没有多租户语义；cron 也把事件归属 `SessionId::main()` 好让主视图显示 —— `main` 就是
   「用户那一条对话」，这个意图已经在代码里。
3. OpenClaw 默认无状态是因为它跨 agent / 跨认证主体路由（authSubject、agentId、scopes 都参与
   会话作用域匹配），没有唯一显然的落点。那套约束在这里不存在，照搬默认值反而是错的。

**代价**（选 `main` 就得接受）：一个会话是单车道，走 `main` 的 HTTP 请求与 TUI 抢同一条队列，
队列满了直接 `Rejected`；两者也共享上下文预算，自动化流量会推着 `main` 提前 compact。高频
自动化应显式传一个自己的 session key。

### 2.2 `instructions` → 追加而非替换

memclaw 的人格来自 SOUL.md，是固定的。`instructions` 不覆盖它，而是作为「本次请求的指令」
追加进去。这样既满足了每请求不同系统提示的需求，也不会让调用方悄悄抹掉人格设定。

### 2.3 `input_file` → 作为不可信内容注入

不改 `Message { content: String }` 结构（改动面太大）。文件解码后拼进本轮文本，用显式边界
标记包住，并标注来源：

```
<<<EXTERNAL_UNTRUSTED_CONTENT id="file_...">>>
Source: External
Filename: notes.txt

...文件内容...
<<<END_EXTERNAL_UNTRUSTED_CONTENT id="file_...">>>
```

边界标记的作用是让模型把这段当**数据**而非指令，降低借文件内容做提示注入的效果。

三者都由 `build_turn_prefix()` 拼成前缀，与用户消息一起走 `chat.send` 的 `text` 字段——
线上协议不必扩字段。

---

## 3. SSE 事件映射

OpenAI 的流式响应把一次输出拆成 `added → delta → done` 三阶段，且每个事件都要
`sequence_number` / `item_id` / `output_index` / `content_index`。`SseState` 维护这些。

事件按 **run_id 精确过滤**：只按 session 过滤会混入并发 run 和 cron 触发的后台 run。

**为什么必须过滤**：daemon 把事件发给**每条连接**（`conn.rs` 的事件转发任务订阅全局
EventBus），所以一条 HTTP 连接会看到 TUI、cron、其他 HTTP 请求的全部事件。

`Usage` 是例外，它**没有 run_id**，只有 session。oc-server 刻意把它留在广播上
（`sink.rs`：只有内联 run 事件挪进了 per-run 定向通道），所以只能按 **session** 过滤。
不过滤的后果是别人的 token 计数覆盖本响应的 usage。同会话内并发 run 仍无法区分 ——
要根治得给 `Event::Usage` 加 run_id，属协议改动。

| oc-proto Event | 发出的 SSE 事件 |
|---|---|
| `Lifecycle::Start` | `response.created` |
| 首个 `Assistant` delta | `response.output_item.added` + `response.content_part.added` + `response.output_text.delta` |
| 后续 `Assistant` delta | `response.output_text.delta` |
| `Usage`（本 session） | 不单独发事件，累进最终响应的 usage |
| `Lifecycle::End` | `response.output_text.done` + `response.output_item.done` + `response.completed` + `[DONE]` |
| `Lifecycle::Error` | `response.failed` + `[DONE]` |

一次完整流：`created → added → part.added → delta×N → text.done → item.done → completed → [DONE]`。
若整轮无任何文本，跳过 item/text 的 done 事件，直接 `completed`。

非流式走同一套累积逻辑，只是不输出 SSE，在 `Lifecycle::End` 后返回完整 `Response`。

`Lifecycle` / `Assistant` 走 per-run 定向通道（有界背压，**不丢**），所以两条累积路径
都不需要超时兜底：只要 run 起步了，终态事件一定到达。

---

## 3.5 并发语义

**同会话串行。** 一个 session actor 单车道，这是 transcript 顺序合法的前提（并发落库会
让 B 的 user 消息被 A 的回复埋在中间 → provider 400，见 `session.rs` 的 submit 注释）。
所以同一 session 的并发 HTTP 请求会排队，`queue_cap = 16`（`provider_setup.rs`）。
要并行就用不同的 `x-openclaw-session-key`。

**队列满 → 立即报错，不挂死。** `submit` 返回 `None`，dispatch 映射成协议错误，HTTP 侧
得到 5xx。这里曾有个挂死 bug：actor 在队列判定**之前**就回执 run_id，被拒的轮拿到合法
id 却永不产生 Lifecycle 事件，而 HTTP 两条累积路径都是无超时 `recv()` 循环 → 请求永久
挂起。默认会话落 `main` 后所有 HTTP 请求与 TUI 挤同一条队列，触发面被放大。回归测试见
`oc-server/tests/queue_full_reject.rs`。

**审批/ask_user 不会挂死。** HTTP 无法回执审批，但 run 侧的等待 `select!` 叠了 `cancel`
与 `sink.closed()`（`tools_bridge.rs`），客户端断连即收敛。

**并发上限。** `ConnPool` 用 `Semaphore` 限**存活**连接数（`--max-conns`，默认 32）。这是
真正稀缺的资源：每条连接在本进程占 2 个 tokio 任务 + 2 个 32 槽 channel，在 daemon 侧还
占一整套连接处理逻辑（读/写/事件转发三条任务 + 256 槽出站队列）。

许可挂在 `NdjsonConn` 上，**随 drop 归还**，所以流式路径自动被覆盖 —— 那条路径不调
`release`，生成器持有连接直到流结束或客户端断开，届时连接与许可一起释放。

`max_idle`（4）是另一回事：只限**闲置**保留数，且被 clamp 到 `max_conns`。闲置连接**保留
许可**——它们仍是存活连接，若放掉许可，`max_conns` 就只限并发使用数，daemon 侧的连接处理
器数量仍可无界增长。

超限时等待 10s 再返回 **503 `capacity_exceeded`**。两个极端都不取：立即失败会误杀只需等
一轮的请求；无限等待则复现了这个上限本要防的挂死（SSE 流会占着许可几分钟）。

---

## 4. 支持范围

### 4.1 请求参数

| 参数 | 状态 | 说明 |
|---|---|---|
| `input` | ✅ | string，或 items 数组（取最后一条 user 消息为当轮） |
| `instructions` | ✅ | 追加到系统提示 |
| `previous_response_id` | ✅ | 复用会话；未知 id 降级为默认会话 |
| `user` | ✅ | 派生稳定会话（与 `main` 隔离） |
| `stream` | ✅ | SSE / JSON |
| `temperature` | ✅ | best-effort，透传 |
| `max_output_tokens` | ✅ | best-effort，透传 |
| `model` | ⚠️ 忽略 | 实际模型由 daemon 配置决定 |
| `store` | ⚠️ 忽略 | 恒为持久化，无法关闭 |
| `metadata` `reasoning` `truncation` | ⚠️ 忽略 | 接受但不起作用 |
| `tools` `tool_choice` | ❌ 拒绝 | 返回 400；Phase 2 计划支持，见 §6 |
| `background` | ❌ 拒绝 | 无异步任务模型 |
| `top_p` `top_logprobs` | ❌ | `ModelRequest` 未暴露 |
| `text.format` | ❌ | 无结构化输出 |

**忽略与拒绝的区别**：忽略的参数不影响结果正确性（少一个旋钮）；会改变语义的参数一律拒绝，
避免客户端以为生效了。

### 4.2 请求头

| 头 | 状态 | 说明 |
|---|---|---|
| `x-openclaw-session-key` | ✅ | 显式会话路由，见 §2.1。非法值 400 |
| `x-openclaw-agent-id` `x-openclaw-model` | ❌ | 单 agent、模型由 daemon 定，无对应概念 |
| `Authorization` | ⚠️ 忽略 | 当前无鉴权；`oc http` 只绑 loopback |

### 4.3 输入类型

| 类型 | 状态 |
|---|---|
| 纯文本 / `message`（system·developer·user） | ✅ |
| `function_call_output` | ✅ 作为工具结果注入本轮 |
| `input_file`（base64，文本类 MIME） | ✅ 上限 60k 字符 |
| `input_file`（url） | ❌ 缺 SSRF 防护，见 §7 |
| `input_image` / 音频 | ❌ 需改 Message 结构 |

历史 `assistant` 消息被忽略：daemon 的 transcript 已有，重复送入会让上下文出现两份。

### 4.4 端点

| 端点 | 状态 |
|---|---|
| `POST /v1/responses` | ✅ |
| `POST /v1/responses/:id/cancel` | ✅ 映射到 `chat.abort` |
| `GET /v1/responses/:id` | ❌ 需把 transcript 反向重建成 OutputItem |
| `DELETE /v1/responses/:id` | ❌ 需存储层支持软删除 |

---

## 5. 永久边界（不打算做）

这几项与架构冲突，不在计划内：

- **`background: true`** — agent 循环是同步/流式的，没有任务队列与完成回调，无法「202 立即返回 + 轮询」。
- **16 种内置工具**（`file_search` / `code_interpreter` / `computer` / `web_search` / `mcp` / …）— 协议各不相同，需逐个适配；memclaw 自己的 exec/file/web 工具语义也不一致。OpenClaw 同样只支持 function 一种。
- **音频输入输出** — 无音频处理能力。
- **`logprobs` / `top_logprobs`** — `Delta` 流不携带 token 概率。
- **`moderation`** — 无内容审查机制。

---

## 6. 待实现（Phase 2 / 3）

以下都是取舍问题而非做不到，按优先级排。

### Phase 2

**动态 `tools`（client-side function tools）** — 最有价值的一项，能让 SDK 的 function
calling 跑通。语义是：工具定义透传给模型，模型产出 `function_call` 后**不执行**，直接返回给
客户端；客户端执行完用 `function_call_output` 回喂下一轮。

已就位的部分：`ModelRequest.tools` 字段存在；`InputItem::FunctionCallOutput` 已能解析并注入
本轮（见 `adapter.rs`）；`OutputItem::FunctionCall` 类型已定义。

缺的是「不执行」这条路径：当前 `run.rs` 收到 `Delta::ToolCall` 后一律走 `exec_tool` 本地执行。
需要区分「daemon 自己的工具」与「客户端声明的工具」，后者跳过执行、把参数原样带出，并在
SSE 上补 `response.function_call.arguments.delta/done` 两个事件。这要改动 oc-server 的 run
驱动，是本项主要成本。同时放开 `adapter.rs::reject_unsupported()` 里对 `tools` / `tool_choice`
的 400。

**`GET /v1/responses/:id`** — transcript 是扁平的 `(seq, role, content)`，要重建成嵌套的
OutputItem 数组。另需持久化 `response_id → (session, seq 区间)`，否则无法定位某次响应的边界
（与 §8 第一条技术债同源，一起做更省）。

**`reasoning` 内容** — DeepSeek-reasoner 的 `Delta::Reasoning` 当前只用于工具轮回喂，未外发。
映射到 `response.reasoning_text.delta/done` 即可，成本低。

**`input_image`** — 需把 `Message.content` 从 `String` 改成 `ContentPart[]`，牵动
oc-llm / oc-server / compaction 的 token 估算。改动面最大，且依赖模型侧视觉能力。

### Phase 3

- **`DELETE /v1/responses/:id`** — 需存储层支持软删除（`entry` 表加 `deleted_at`）。
- **`text.format: json_object`** — 非 schema 校验的宽松模式，依赖模型配合。
- **`include` 字段过滤** — 当前恒返回全部可用字段。

### 生产化（与功能无关，部署前需要）

- 持久化 `response_id → SessionId`（见 §8）。
- Bearer token 鉴权 —— 当前完全无鉴权，见 §7。
- 限流。

---

## 7. 安全边界

- **仅监听 127.0.0.1，且无鉴权**。该端点等同于对 daemon 的完全操作权（含 exec 工具）。要远程访问，前面必须加带认证的反向代理，不要直接改绑 `0.0.0.0`。
- **`input_file` 的 url 源被拒绝**。实现 URL 拉取需要 DNS 解析检查、私有网段拦截、重定向跳数限制、超时——缺一个就是 SSRF 通道。当前要求调用方自己读文件后传 base64，把这层攻击面整个去掉。
- **文件内容视为不可信**，用边界标记包裹（§2.3）。
- **无限流**。生产部署应在代理层加。

---

## 8. 已知技术债

- `response_id → SessionId` 在内存里，重启后 `previous_response_id` 失效。要持久化需加一张表并走 schema 迁移，暂未做。
- 流式路径不把连接**放回池**复用（许可已随 drop 归还，见 §3.5，但连接本身重建）。要复用需在流结束时把 conn 交还池，而 `Sse` 拿走了所有权，改动不小、收益有限（省一次 connect + handshake）。
- 池化连接复用时可能带最多 288 个陈旧帧（本地 32 + daemon 出站 256）。`handshake` 跳过非 `Res` 帧、`run_id`/session 过滤挡住事件，故不影响正确性，但白读一遍。
- 等待许可的请求本身不占连接，但仍占一个 tokio 任务和已解析的请求体（axum 默认 2MB 上限）。极端洪峰下内存随等待者数量增长；真要防得在 axum 层加 `ConcurrencyLimitLayer` 或前置代理限流。
- 同会话内并发 run 的 `Usage` 无法精确归属（`Event::Usage` 没有 run_id）。跨会话已按 session 过滤。
- 输出 token 用 `chars/4` 估算——daemon 的 `Usage` 事件只报 input_tokens。比报 0 好，但不准。
- 错误码映射较粗：`LoopDetected` / `Timeout` 目前都归到 5xx，可细化为 429 / 504。队列满目前也是 5xx（`ErrorKind` 无 busy 变体），语义上更该是 429/503。

---

## 9. 验证状态

`cargo test -p oc-http`：41 个单元测试，覆盖

- **会话解析** — user 稳定性与 `main` 隔离、裸请求落 `main`、`previous_response_id` 命中与
  未命中降级（降级仍保留 user 作用域）
- **session key 校验** — 头优先于 user/`previous_response_id`、trim、空值/超长/控制字符/保留
  前缀拒绝（且只匹配前缀不匹配子串）、非 UTF-8 头拒绝
- **input 归一化** — 多条 user 取最后一条、system 进提示、空输入拒绝、`function_call_output` 单独成轮
- **文件处理** — base64 各余数长度的解码、不可信边界包裹、url 源拒绝、非 function 工具拒绝
- **跨会话隔离** — 别的 session 的 `Usage` 不污染本响应的 token 计数
- **SSE 状态机** — 跨 run 事件隔离、首 delta 开 item、序列号单调、三种终止路径（正常/无文本/错误）

另有 `oc-server/tests/queue_full_reject.rs` 守住队列满必须返回 `None`（该测试在修复前
确实失败，验证过），`oc-http/src/conn_pool.rs` 守住并发上限语义（许可计数、超限报 Busy、
drop 归还许可）。

全量 `cargo test`：263 passed。`cargo clippy --workspace --all-targets`：无告警。

尚未做**真机端到端验证**（起 daemon + `oc http`，用真实 OpenAI SDK 打通）。
按本仓历史，P0-1 / P1-1 的缺陷都是真机才暴露的，这一步不能省。
