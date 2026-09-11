# 架构概览

oc 是一个 Rust 编写的个人助手 daemon，常驻后台，通过终端 UI（TUI）或 HTTP 与用户对话，接 OpenAI 兼容模型（DeepSeek、Anthropic 等），具备分层记忆和主动性（定时任务、话题触发提醒）。

---

## crate 划分

依赖方向单向：上层 crate 依赖下层，禁止循环依赖。

```
oc-cli  oc-tui  oc-http
    └───────┬───────┘
        oc-server
        ├── oc-tools
        ├── oc-llm
        ├── oc-store
        └── oc-core
            └── oc-proto
```

| crate | 职责 |
|---|---|
| `oc-proto` | 交互协议 DTO，JSON schema，所有 crate 的共享类型 |
| `oc-core` | 领域层，纯函数，**无 IO**（config / agent 状态机 / 记忆排名 / dreaming / cron / intent / prompt 组装 / loop detection）|
| `oc-store` | 存储层，SQLite WAL + FTS5，单写线程 + 读连接池 |
| `oc-llm` | 模型传输，SSE 流解析，Anthropic 与 OpenAI 两类 provider 归一为 `Delta` 流 |
| `oc-tools` | 7 个工具（file / shell / sys / ask_user 等）+ 审批门 |
| `oc-server` | 编排层：会话管理、Agent 循环、事件广播、core + store + llm + tools 在此接线 |
| `oc-http` | OpenAI Responses API 兼容层，独立进程，连到已在运行的 `oc-server` |
| `oc-tui` | 终端 UI，Ratatui |
| `oc-cli` | 命令行入口，`oc` 二进制，clap 解析，进程启动与 IPC 连接 |

---

## 核心不变量

每个版本都必须维持，违反即 bug：

1. **`oc-core` 无 IO**：领域逻辑全部是纯函数，可在单元测试里注入时钟、随机、配置，无需 mock 外部系统。
2. **依赖单向无环**：上层可依赖下层，反向依赖不允许（CI clippy 间接保证）。
3. **记忆 / dreaming / 主动性失败不阻塞主会话回复**：这三条路径的错误被捕获并记录，不传播到对话 run。
4. **prompt 组装确定性**：相同配置 + 相同历史 → 字节稳定的 prompt 输出，工具/技能/记忆按稳定键排序注入，便于 prefix caching 命中。

---

## 进程模型

```
oc serve          ←── daemon，常驻，单实例锁（~/.oc/run/oc.lock）
    ↑ unix socket / named pipe
oc tui            ←── TUI 客户端，临时进程，用完退出
oc http [可选]    ←── HTTP 网关，独立进程，连到同一个 daemon
```

- daemon 与客户端通过 **AF_UNIX socket**（Linux/macOS，`~/.oc/run/oc.sock`）或 **Windows 命名管道**（`\\.\pipe\oc-daemon`）通信，可用 `OC_SOCKET` 环境变量或 `--socket` 参数覆盖。
- `oc http` 崩溃不影响 daemon 和已有会话。

---

## 会话车道模型

每个 session 一条车道，同一会话同一时间只有一个 run 在执行（单开）。新消息在上一个 run 结束前排队，不并发执行。

特殊会话独立车道，不占 `main`：

| 会话 | 用途 |
|---|---|
| `main` | 用户主会话 |
| `cron/<id>` | cron 定时任务触发 |
| `dreaming` | 夜间记忆巩固 |
| HTTP session key | `x-openclaw-session-key` 指定的会话 |

---

## 存储

单个 SQLite 文件（`~/.oc/oc.sqlite`），WAL 模式：

- **写线程**：唯一持有可写连接（`WriterActor`），所有写操作串行排队，crash 安全，写线程死亡后降级为只读（见 [ADR-0001](adr/0001-per-run-sink-backpressure.md) 的背景说明）。
- **读连接池**：`reader.rs` 维护小池，读操作走 `spawn_blocking`，`PRAGMA query_only=ON` 防止读连接意外写入，读写互不阻塞（WAL 保证）。
- **全文索引**：`memory.text` 用 FTS5 加速检索，中文采用 2-gram 预切词策略（见 [ADR-0002](adr/0002-fts5-trigram-tokenization.md)）。
