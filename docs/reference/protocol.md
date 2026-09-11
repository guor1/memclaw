# HTTP 接口参考（OpenAI Responses API 兼容）

`oc http` 是独立进程，在 daemon 前面加一层 HTTP 适配器。HTTP 侧崩溃不影响核心会话。

## 启动

```bash
oc http [--port 8080] [--socket <路径>] [--max-conns 32]
```

绑定 `127.0.0.1`。

| 参数 | 默认 | 说明 |
|---|---|---|
| `--port` | `8080` | HTTP 监听端口 |
| `--socket` | 平台默认 | oc-server 的 socket / 管道路径 |
| `--max-conns` | `32` | 到 daemon 的并发连接硬上限，超出后新请求等 10s 再返回 503 |

---

## POST /v1/responses

请求体映射到内部 NDJSON 协议：

| OpenAI 字段 | 处理 |
|---|---|
| `input` | 用户消息（支持字符串或 content 数组，含 base64 文件输入）|
| `instructions` | 注入为系统提示词 |
| `stream` | `true` 走 SSE，`false` 一次性返回 |
| `user` | 会话路由键 |
| `previous_response_id` | 会话延续 |
| 其他不支持的参数 | **显式返回 400**，不静默降级 |

「不支持就报 400 而不是静默忽略」是刻意的——静默降级会让客户端以为参数生效了。

---

## 会话路由

三种方式指定会话，优先级从高到低：

1. `x-openclaw-session-key` 请求头
2. 请求体 `user` 字段
3. `previous_response_id`（延续该响应所属会话）

都不给则落到 `main` 会话。

**自动化脚本建议显式指定 session key**，别挤 `main`——那是你自己在 TUI 里用的会话。

同一会话本就串行执行（车道模型），调大 `--max-conns` 只对多 session key 并行有意义。

---

## SSE 流式

`stream: true` 时返回 `text/event-stream`，事件类型对齐 OpenAI Responses API 的 `response.*` 系列。

---

## 安全边界

绑定 `127.0.0.1`，**没有认证**。

这是单用户本地 daemon 的设计前提。不要暴露到公网或放在反向代理后面——任何能访问该端口的进程都能以你的身份对话、跑工具、读写你的记忆。
