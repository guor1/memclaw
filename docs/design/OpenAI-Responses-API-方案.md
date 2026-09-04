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
新会话而不是报错，因为「继续对话」的意图用新会话仍能满足，报 404 反而更糟。

优先级：`previous_response_id` → `user` 派生 → 每请求独立。

```rust
// adapter.rs
pub fn resolve_session(req: &CreateResponseReq, sessions: &ResponseSessions) -> SessionId {
    if let Some(prev) = &req.previous_response_id {
        if let Some(sid) = sessions.get(prev) { return sid.clone(); }
    }
    if let Some(user) = &req.user {
        // 同一个 user 稳定映射到同一会话，跨请求共享上下文
        return SessionId::new(format!("http-user-{:x}", hash(user)));
    }
    SessionId::new(format!("http-{}", uuid::Uuid::now_v7()))  // 默认无状态
}
```

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

| oc-proto Event | 发出的 SSE 事件 |
|---|---|
| `Lifecycle::Start` | `response.created` |
| 首个 `Assistant` delta | `response.output_item.added` + `response.content_part.added` + `response.output_text.delta` |
| 后续 `Assistant` delta | `response.output_text.delta` |
| `Usage` | 不单独发事件，累进最终响应的 usage |
| `Lifecycle::End` | `response.output_text.done` + `response.output_item.done` + `response.completed` + `[DONE]` |
| `Lifecycle::Error` | `response.failed` + `[DONE]` |

一次完整流：`created → added → part.added → delta×N → text.done → item.done → completed → [DONE]`。
若整轮无任何文本，跳过 item/text 的 done 事件，直接 `completed`。

非流式走同一套累积逻辑，只是不输出 SSE，在 `Lifecycle::End` 后返回完整 `Response`。

---

## 4. 支持范围

### 4.1 请求参数

| 参数 | 状态 | 说明 |
|---|---|---|
| `input` | ✅ | string，或 items 数组（取最后一条 user 消息为当轮） |
| `instructions` | ✅ | 追加到系统提示 |
| `previous_response_id` | ✅ | 复用会话；未知 id 降级为新会话 |
| `user` | ✅ | 派生稳定会话 |
| `stream` | ✅ | SSE / JSON |
| `temperature` | ✅ | best-effort，透传 |
| `max_output_tokens` | ✅ | best-effort，透传 |
| `model` | ⚠️ 忽略 | 实际模型由 daemon 配置决定 |
| `store` | ⚠️ 忽略 | 恒为持久化，无法关闭 |
| `metadata` `reasoning` `truncation` | ⚠️ 忽略 | 接受但不起作用 |
| `tools` `tool_choice` | ❌ 拒绝 | 返回 400，见 §5 |
| `background` | ❌ 拒绝 | 无异步任务模型 |
| `top_p` `top_logprobs` | ❌ | `ModelRequest` 未暴露 |
| `text.format` | ❌ | 无结构化输出 |

**忽略与拒绝的区别**：忽略的参数不影响结果正确性（少一个旋钮）；会改变语义的参数一律拒绝，
避免客户端以为生效了。

### 4.2 输入类型

| 类型 | 状态 |
|---|---|
| 纯文本 / `message`（system·developer·user） | ✅ |
| `function_call_output` | ✅ 作为工具结果注入本轮 |
| `input_file`（base64，文本类 MIME） | ✅ 上限 60k 字符 |
| `input_file`（url） | ❌ 缺 SSRF 防护，见 §6 |
| `input_image` / 音频 | ❌ 需改 Message 结构 |

历史 `assistant` 消息被忽略：daemon 的 transcript 已有，重复送入会让上下文出现两份。

### 4.3 端点

| 端点 | 状态 |
|---|---|
| `POST /v1/responses` | ✅ |
| `POST /v1/responses/:id/cancel` | ✅ 映射到 `chat.abort` |
| `GET /v1/responses/:id` | ❌ 需把 transcript 反向重建成 OutputItem |
| `DELETE /v1/responses/:id` | ❌ 需存储层支持软删除 |

---

## 5. 无法支持的部分

分两类。**架构级**的不打算做：

- **`background: true`** — agent 循环是同步/流式的，没有任务队列与完成回调，无法「202 立即返回 + 轮询」。
- **16 种内置工具**（`file_search` / `code_interpreter` / `computer` / `web_search` / `mcp` / …）— 协议各不相同，需逐个适配；memclaw 自己的 exec/file/web 工具语义也不一致。OpenClaw 同样只支持 function 一种。
- **音频输入输出** — 无音频处理能力。

**待实现**的是取舍问题，不是做不到：

- **动态 `tools`（client-side function tools）** — 工具定义透传给模型，模型产出 `function_call` 后**不执行**，返回给客户端，客户端执行完用 `function_call_output` 回喂。`ModelRequest.tools` 已有字段，`function_call_output` 的解析也已就位，缺的是 `Delta::ToolCall` 不执行而直接出参的那条路径。
- **`input_image`** — 需把 `Message.content` 从 `String` 改成 `ContentPart[]`，牵动 oc-llm/oc-server/compaction。
- **`GET /v1/responses/:id`** — transcript 是扁平的 `(seq, role, content)`，要重建成嵌套 OutputItem。

---

## 6. 安全边界

- **仅监听 127.0.0.1，且无鉴权**。该端点等同于对 daemon 的完全操作权（含 exec 工具）。要远程访问，前面必须加带认证的反向代理，不要直接改绑 `0.0.0.0`。
- **`input_file` 的 url 源被拒绝**。实现 URL 拉取需要 DNS 解析检查、私有网段拦截、重定向跳数限制、超时——缺一个就是 SSRF 通道。当前要求调用方自己读文件后传 base64，把这层攻击面整个去掉。
- **文件内容视为不可信**，用边界标记包裹（§2.3）。
- **无限流**。生产部署应在代理层加。

---

## 7. 已知技术债

- `response_id → SessionId` 在内存里，重启后 `previous_response_id` 失效。要持久化需加一张表并走 schema 迁移，暂未做。
- 非流式路径下 `accumulate_response` 结束后连接才归还池；流式则由 SSE 持有到流结束。
- 输出 token 用 `chars/4` 估算——daemon 的 `Usage` 事件只报 input_tokens。比报 0 好，但不准。
- 错误码映射较粗：`LoopDetected` / `Timeout` 目前都归到 5xx，可细化为 429 / 504。

---

## 8. 验证状态

`cargo test -p oc-http`：22 个单元测试，覆盖

- **会话解析** — user 稳定性、无 user 时的独立性、`previous_response_id` 命中与未命中降级
- **input 归一化** — 多条 user 取最后一条、system 进提示、空输入拒绝、`function_call_output` 单独成轮
- **文件处理** — base64 各余数长度的解码、不可信边界包裹、url 源拒绝、非 function 工具拒绝
- **SSE 状态机** — 跨 run 事件隔离、首 delta 开 item、序列号单调、三种终止路径（正常/无文本/错误）

全量 `cargo test`：229 passed。`cargo clippy -p oc-http --all-targets`：无告警。

尚未做**真机端到端验证**（起 daemon + `oc http`，用真实 OpenAI SDK 打通）。
按本仓历史，P0-1 / P1-1 的缺陷都是真机才暴露的，这一步不能省。
