# Rust 落地方案 v2：专用个人助手版

> 本文取代 [03-Rust落地方案.md](03-Rust落地方案.md)。03 是"精简版通用网关"视角；本版按**真正意义上的个人助手**重新定位。
> 定位判据只有一句：**"它能不能像一个长期陪着我的助手一样工作？"**——覆盖多少场景不重要，记不记得住我、有没有主动性、会不会卡死才重要。
>
> 三个已定决策：
> - 交互 = **本地 CLI/TUI + 一套定义清晰的消息协议**（不绑定具体渠道；渠道以后当适配器接入）
> - 记忆 = **完整形态**（Dreaming / 主动巩固 / standing intents / 跨会话回忆 全进 MVP）
> - 主动性 = **进 MVP**（cron + heartbeat + 主动提醒）

---

## 0. 设计原则

1. **单用户假设贯穿一切**：你是唯一用户。这消灭了大量复杂度——不需要 DM 隔离、多租户、多 agent 路由、bindings、复杂 scope 模型。
2. **运行时最小 + 灵魂厚**：砍生态层（插件市场/多 provider/多渠道/云 worker），把省下的复杂度预算投到记忆与主动性。
3. **单二进制 + tokio**：一个 `oc` bin，`oc serve` 常驻，`oc` 直接进 TUI 对话。
4. **协议先行**：交互层不写死渠道，而是定义一套干净的消息协议；CLI/TUI 是它的第一个 client，未来 Telegram 等只是协议适配器。
5. **core 无 IO / 边界即 trait**：领域逻辑可纯单测。
6. **在你信任的机器上跑**：所以 exec 审批门足够，**沙箱诚实留在 Phase 2**（不是不可信环境，不急）。

---

## 1. 砍 / 留 / 提前（相对通用网关）

| 大胆砍掉（纯生态包袱） | 保留（助手核心） | 从后置提进 MVP（个人助手的灵魂） |
|---|---|---|
| 多 agent 路由 + bindings | SQLite 存储（双库→**单库够用**，见 §2） | 完整记忆：Dreaming / User model / standing intents / trigger 注入 / 跨会话回忆 |
| DM scope（固定单会话/主会话） | agent 循环 + 队列 + steer/interrupt | cron 定时任务 |
| 插件系统 + SDK + ClawHub | LLM 传输（OpenAI+Anthropic 兼容） | heartbeat 心跳 + 主动提醒 |
| 多租户 + cloud worker + worker 协议 | 工具集 exec/file/web/message/process | SOUL.md 人格 |
| 协议版本协商 / N-1 兼容 | 压缩 compaction + 剪枝 | loop detection + 卡死会话恢复 |
| 35+ provider（留 1~2） | 系统提示词组装 + bootstrap 注入 | background tasks 台账 |
| 25+ 渠道（换成"协议 + CLI"） | exec 审批门 + 工具策略 | 空闲看门狗 / run 超时 / 中止（防卡死第一闸） |
| operator/node/worker scope 模型 | skills 加载/注入 | prompt cache 确定性排序 |
| 大部分 gateway RPC（给 UI/SDK 的） | audit ledger | |
| 沙箱（→ Phase 2，信任环境不急） | | |

**结果**：更小的运行时（无生态层）+ 更厚的助手灵魂（记忆+主动性+人格）。

---

## 2. Workspace 与模块划分

```
oc/
├── Cargo.toml                 # [workspace]
├── crates/
│   ├── oc-core/               # 纯领域逻辑，无 IO
│   │   ├── session/           # 单会话/主会话模型、生命周期、重置（无多 agent/无 DM scope）
│   │   ├── agent/             # agent loop 状态机、终态归一化、回复整形
│   │   ├── queue/             # 队列 + steer/followup/collect/interrupt + 卡死恢复
│   │   ├── prompt/            # 系统提示词渲染（纯函数）、bootstrap、SOUL.md 人格、cache 边界
│   │   ├── context/           # 上下文引擎 trait + legacy、token 预算
│   │   ├── compaction/        # 压缩、overflow 模式表、split-point 配对、压缩前记忆 flush
│   │   ├── tool/              # Tool trait、策略管道、结果净化、loop detection
│   │   ├── memory/            # ★纯策略：分层规则 + provenance 分类 + Lane1 排名公式 + trigger 预筛判定 + dreaming 双门判定 + User model supersede + standing intents 预筛（SQL/向量执行在 oc-store）
│   │   ├── model/             # 模型目录、别名、failover、代际快照
│   │   ├── proactive/         # ★纯策略：下次触发时间计算 + cron 表达式求值 + anti-nagging 判定（tokio timer/spawn 在 oc-server）
│   │   └── config/            # config 类型、校验、SecretRef、reloadKind
│   ├── oc-store/              # SQLite 单库、迁移、写队列 actor、向量索引
│   ├── oc-llm/                # Provider trait + OpenAI/Anthropic 兼容、SSE 流
│   ├── oc-proto/              # ★消息协议 DTO + JSON Schema（交互层契约）
│   ├── oc-server/             # 常驻进程：协议服务端（stdio/WS/unix sock）+ 事件广播 + 后台任务台账
│   ├── oc-tools/              # exec/process/file/web_fetch/web_search/ask_user/message
│   ├── oc-tui/                # 本地终端 UI（协议第一个 client）
│   └── oc-cli/                # clap 命令树 + serve/doctor/onboard
└── bin: oc
```

**依赖方向**（单向无环）：oc-server 是唯一的组装/编排层，向下拉取 core（纯策略）、store（持久化）、tools、llm；Lane2 子智能体跨历史检索由 oc-server 编排、oc-core 出策略、oc-store 执行查询。

```
oc-cli ─► oc-tui ─► oc-server ─► oc-core
                       │  │  │      ▲
                       │  │  └────► oc-tools ─► oc-llm   # 工具/provider 由 server 注入组装
                       │  └───────► oc-llm              # agent 循环直接调 provider
                       └──────────► oc-store            # 持久化 + SQL/向量执行
                           oc-proto ◄── (oc-server / oc-tui 共享)
```

**存储简化**：单用户下不需要"共享 state DB + per-agent DB"两库。**MVP 用一个 `oc.sqlite`** 装全部（会话/transcript/记忆索引/记忆向量/cron/intents/任务台账/审批/config 状态）。schema 仍版本化 + 前向迁移 + 单写线程。

---

## 3. 交互层：消息协议（你的第 1 个决策）

不写死渠道，定义一套干净协议；CLI/TUI 是首个 client，未来渠道是适配器。

### 3.1 传输
- MVP：**Unix domain socket / Windows named pipe**（本地 CLI↔常驻进程，最省、无网络面）+ 可选 **WebSocket**（feature，给未来远程/网页 client）。
- 帧沿用 OpenClaw 三类：`req` / `res` / `event`（JSON）。单用户下砍掉握手 challenge 签名、scope 模型、设备配对；本地 socket 即信任边界。可选 token 用于 WS 远程。

### 3.2 核心方法（MVP 最小集）
```
connect            → hello(features, snapshot)
chat.send          → {runId}          # 发一条消息，立即返回
chat.abort         → {}               # 打断（防卡死 / 用户主动停）
chat.history       → [entries]
session.reset      → {}               # /new /reset
cron.add/list/rm   → ...              # 主动性
tasks.list/cancel  → ...              # 后台任务台账
memory.search      → [hits]           # 调试/自省用
status / health    → snapshot
```

### 3.3 事件流（服务端推）
```
lifecycle(start|end|error)   # run 生命周期
assistant(delta)             # 流式回复
tool(start|update|end)       # 工具活动
proactive(reminder|wake)     # ★主动提醒推送
task(update)                 # 后台任务进展/完成
```

### 3.4 协议契约要求
- DTO 全在 `oc-proto`，`serde` + `schemars` 生成 JSON Schema（即使只有 CLI 也生成，作为渠道适配器的接入契约）。
- 判别联合（tagged enum）表达帧/事件，让"不可能的状态无法表示"。
- side-effecting 方法（`chat.send`）带幂等键。

**→ Rust**：`tokio` unix socket / `tokio-tungstenite`（WS feature）。事件用 `tokio::sync::broadcast`，每 client 独立订阅。

---

## 4. 记忆系统：完整形态（你的第 2 个决策）

这是本版相对 03 的最大加码。对个人助手，记忆是灵魂，不是加分。

### 4.1 分层（照搬机制文档，落到单库表）
| Tier | 载体 | 写入者 | 注入 |
|---|---|---|---|
| Instructions | AGENTS.md/SOUL.md/USER.md | 人 | 会话起始 |
| Curated core | MEMORY.md, USER.md | dreaming / 用户显式 | 会话起始，有预算 |
| Episodic | memory/YYYY-MM-DD.md, transcripts | agent / flush / 转写 | 从不；按需搜 |
| Prospective | standing intents(表) + cron | intent 工具 / 调度 | 触发时 |
| Review | DREAMS.md | dreaming | 从不；给人读 |

### 4.2 Provenance（抗投毒，必做）
`origin` 枚举 `Owner|Agent|Untrusted|System` 存 SQLite 列，写入端分类，**绝不默认 owner**；无法判定 → 保守 untrusted/system。cron/heartbeat/subagent 会话不产生持久候选。召回环防护：注入过的内容结构性标记，不再被重抽取。

### 4.3 召回两 lane
- **Lane1（零模型调用，延迟敏感）**：bootstrap 注入 + 排名搜索（相关性 × 30天半衰期 × importance）+ trigger 注入（词法/向量预筛 ≥0.72，≤3/轮）。自动注入**仅限 curated tier**。
- **Lane2（升级）**：子智能体跨历史检索（含跨会话回忆），仅当显示召回意图且 Lane1 无强命中。落点：**oc-server 编排子会话，oc-core 出检索策略，oc-store 执行查询**。

### 4.4 Dreaming（后台巩固，本版进 MVP）
后台三阶段：light/REM 暂存反思 → deep 双门升级（确定性门排名 + 结构排除 untrusted/system → 巩固模型轮重写 MEMORY.md）。写安全用乐观并发（内容 hash 重校验 + 原子 rename），失败回退 append-only。触发由 **M3 的心跳 tick** 驱动（夜间/空闲），M6 起纳入 `proactive` 统一管理；双门判定逻辑在 `oc-core`（纯，可单测），文件/DB 读写落 `oc-store`。

### 4.5 User model + standing intents
- USER.md：偏好指令，就地 supersede（不 append 矛盾项）。
- standing intents：事件型待办进 SQLite 表（keywords/trigger 向量/scope/expiry/budget/cooldown）；每条入站消息确定性预筛，命中注入隐藏上下文。时间型待办 → 编译成 cron。

**→ Rust**：Lane1 排名/预筛公式在 `oc-core`（纯，可单测），SQL + 向量查询落 `oc-store`，由 `oc-server` 调用时每步套 `tokio::time::timeout` + fallback（记忆失败绝不阻塞回复）。向量用 `sqlite-vec`（本版**默认开**，因记忆是核心）。dreaming 判定在 core，其调度任务由 `oc-server` 挂到 M3 心跳 tick 上。

---

## 5. 主动性（你的第 3 个决策）

纯策略在 `oc-core::proactive`（触发时间计算、cron 求值、anti-nagging 判定），进程内调度由 `oc-server` 承载，无外部依赖。**调度底座在 M3，主动性语义在 M6**（见 §9）。

| 能力 | 说明 |
|---|---|
| **cron** | "周五提醒我""每天早上总结"。独立隔离会话，own timer + 到期 abort + 清理。**语义在 M6** |
| **heartbeat** | 心跳节拍，默认 agent 有 heartbeat prompt/ack，驱动 dreaming、standing intent 复检、卡死会话恢复扫描。**最小 tick 底座在 M3**（供 M4 卡死扫描 / M5 dreaming 挂载），M6 纳入 proactive 统一管理 |
| **主动提醒** | intent/cron 触发 → 经协议 `proactive` 事件推给 client；CLI/TUI 显示，未来渠道适配器转发 |
| **wake** | 立即/下次心跳注入唤醒文本 |
| **anti-nagging** | intent 默认 cooldown 24h、budget 3 次、90 天过期、≤3/轮 |

**→ Rust**：`oc-server` 里一个 `tokio` 调度 task 持有最小堆（下次触发时间），触发时间/表达式求值调 `oc-core::proactive`（`cron` crate 解析）；触发 → 走正常 agent 轮（隔离会话）→ 结果经 `proactive` 事件推送。heartbeat 的最小固定间隔 tick 在 M3 就落在 oc-server，M4/M5 把卡死扫描器、dreaming 触发器挂上去，M6 再补 cron/anti-nagging 的完整语义。

---

## 6. 防卡死（复杂任务不崩，分层兜底）

对齐前面讨论，本版把这些全列为**验收项**，不是性能备注。

| 死法 | 机制 | 里程碑 |
|---|---|---|
| 模型不吐 token | 空闲看门狗 cloud 120s/自托管 300s | M3 |
| run 跑太久 | agent 超时（默认长、可配、0=无限）+ abort timer | M3 |
| 工具卡住 | 工具级超时 + `process` 转后台/可 kill | M4 |
| 模型打转 | loop detection | M4 |
| 上下文爆 | 压缩 + overflow→压缩重试 + 结果剪枝 | M5 |
| 会话车道被占死 | 卡死诊断（long_running/stalled/stuck）+ abort 阈值 drain 释放车道 | M4 |
| 用户要打断 | `chat.abort`（先清排队轮再中止活跃 run）+ `/stop` | M4 |
| 崩溃/重启后 | transcript 落库、下条消息续、background task 台账 push 完成 | M4/M5 |

**慢 ≠ 卡**：abort 阈值 ≥5min 且 ≥3× 警告阈值，慢 run 在阈值前保持 long_running 不误杀。

**panic 隔离**：单个 run/工具 panic 被 run 边界的 `catch_unwind` 捕获，转成该 run 的错误终态，**不拖垮常驻进程**（前提：profile 用 `panic="unwind"`，见 §7）。里程碑 M4，与 loop detection / 卡死诊断同批验收。

---

## 7. Crate 选型（个人助手版最小集）

相对 03 的差异：**去掉多渠道/多 provider 相关**，**向量记忆默认开**，**去掉 r2d2**（单用户单写、读也可直接开连接）。

| 用途 | Crate | 备注 |
|---|---|---|
| runtime | `tokio`（`rt-multi-thread,macros,sync,time,io-util,net,signal`，不开 full） | |
| 异步工具 | `futures-util` | |
| 并发容器 | `dashmap` | 幂等缓存 |
| 原子快照 | `arc-swap` | 代际快照 / config 热更 |
| 取消 | `tokio-util`（CancellationToken） | 防卡死 |
| 本地 IPC | `tokio`（unix socket / named pipe） | 交互主通道 |
| WS（可选 feature） | `tokio-tungstenite` | 远程 client |
| JSON | `serde` + `serde_json` | |
| Schema | `schemars` | 协议契约 |
| 校验 | `garde` | |
| SQLite | `rusqlite`（`bundled`） | **同步写线程，贴合"事务内不 await"** |
| 向量 | `sqlite-vec` | **本版默认开**（记忆核心） |
| HTTP client | `reqwest`（`rustls-tls,stream,json`，关 default） | provider 请求 |
| SSE | 手写 on `bytes_stream` | 省依赖 |
| tokenizer | 近似(字符/4)，`tiktoken-rs` 为可选 feature | |
| CLI | `clap`（derive） | |
| TUI | `ratatui` + `crossterm` | 本地终端 UI |
| 交互提示 | `inquire` | onboard |
| 日志 | `tracing` + `tracing-subscriber` | |
| 哈希 | `sha2` | **核心必需**：dreaming 内容 hash 重校验（§4.4）、幂等键 |
| 签名（WS 远程才需） | `ed25519-dalek`、`hmac` | 本地 socket 可免签名 |
| 随机/ID | `rand`、`uuid`(v4+v7) | |
| 时间 | `time` + `time-tz` | 更小 |
| cron | `cron` | 主动性 |
| 文件锁 | `fs2` | 单实例锁 |
| 路径/原子写/glob | `directories`、`tempfile`、`globset` | |
| 沙箱（Phase 2） | `bollard`（feature，默认关） | 信任环境不急 |

非可选核心直接依赖 ~28 个（含 `sha2` 等），WS/沙箱/tokenizer 相关为可选。三大压依赖杠杆不变：tokio 不开 full、reqwest 关 default、SQLite bundled。

编译剖面（release）：`opt-level="s"` + `lto=true` + `codegen-units=1` + `strip=true`，**`panic="unwind"`**。
> 不用 `panic="abort"`：本 daemon 靠 run 边界 `catch_unwind` 隔离单次 panic（§6），abort 会让任一工具 panic 杀掉整机，与"复杂任务不崩"矛盾。`opt-level` 用 `s` 而非 `z`：个人助手不以二进制体积为主约束，`z` 偶尔拖慢 Lane1 等热路径。

---

## 8. Feature gate

```toml
[features]
default = ["provider-openai", "provider-anthropic", "memory-vec"]

provider-openai    = ["oc-llm/openai"]
provider-anthropic = ["oc-llm/anthropic"]
memory-vec         = ["oc-store/sqlite-vec"]   # 默认开：记忆是核心

# 可选
ws-remote  = ["oc-server/ws", "dep:tokio-tungstenite"]  # 远程 client
tokenizer  = ["oc-core/tiktoken"]
sandbox    = ["oc-tools/docker"]               # Phase 2
otel       = ["oc-server/otel"]
```

单用户本地默认构建 = OpenAI兼容 + Anthropic兼容 + 向量记忆 + 本地 socket + TUI。不含 WS、沙箱、OTel。

---

## 9. 里程碑（个人助手版，含验收标准）

**M1 — 存储 + 配置 + 协议骨架**
- `oc-store`：单库 `oc.sqlite`、schema 版本化 + 前向迁移、单写线程 + WAL、`sqlite-vec` 就绪
- `oc-core::config` + SecretRef(inline/env/file) + 校验
- `oc-proto`：协议 DTO + JSON Schema 生成；单实例文件锁
- 验收：`oc doctor` 建库通过；协议 schema 可导出

**M2 — 常驻进程 + 交互协议**
- `oc-server`：unix socket 服务端、req/res/event、事件广播、幂等缓存
- `oc-tui`：本地 TUI 连上、能收发、显示流式
- 验收：TUI 里发消息→收到 echo/事件；`oc serve` + `oc`(TUI) 双进程跑通；**AF_UNIX 与 Windows named pipe 是两套 API，双平台各验一次传输通路**

**M3 — Agent 循环 + LLM + 防卡死一闸**
- `oc-core::{agent,queue,prompt,model}` + `oc-llm`（OpenAI/Anthropic 兼容 + SSE + failover + retry）+ 代际快照
- 系统提示词组装 + bootstrap 注入 + SOUL.md 人格
- **最小心跳 tick 底座**：oc-server 里固定间隔 `tokio::time::interval`，仅按时触发回调（不含 cron/anti-nagging），供 M4 卡死扫描 / M5 dreaming 挂载
- 验收：能多轮对话；**空闲看门狗 + run 超时 + `chat.abort` 中止**三项可演示（M3 的 `chat.abort` 只需中止当前活跃 run，供看门狗/超时复用；先 drain 排队轮 + `/stop` 的完整语义见 M4）；**prompt cache 确定性排序**成立；心跳 tick 可观测到按间隔触发

**M4 — 工具 + 审批 + 防卡死全套 + 后台台账**
- `oc-tools`：exec/process/file/web_fetch/web_search/ask_user/message + 工具策略 + exec 审批门
- **loop detection + 卡死会话诊断/abort-drain + background tasks 台账 + 工具超时 + run 边界 panic 隔离（`catch_unwind`）**
- 验收：**"会话→生成脚本→(审批)→执行→结果流式回传"闭环**；模型打转能被打断；卡住的 run 到阈值释放车道；长命令转后台且完成 push；**单工具 panic 不杀进程，转为该 run 错误终态**

**M5 — 上下文 + 压缩 + 技能 + 完整记忆**
- `oc-core::{context(legacy),compaction}` + 压缩前记忆 flush + 结果剪枝
- skills 加载/注入
- **记忆完整形态**：分层 + provenance + Lane1(排名+trigger) + Lane2(跨会话) + Dreaming + User model + standing intents
- audit ledger
- dreaming 触发挂在 **M3 心跳 tick** 上（M6 再纳入 proactive 统一管理）
- 验收：长对话自动压缩不崩；跨会话能回忆；夜间/空闲 dreaming（经心跳触发）巩固 MEMORY.md；"记住我偏好"生效

**M6 — 主动性 + CLI 收口**
- `oc-core::proactive` 语义收口：cron 表达式 + 主动提醒(proactive 事件) + wake + anti-nagging(cooldown/budget/expiry)；把 M3~M5 各处挂在心跳 tick 上的触发（dreaming/卡死扫描/intent 复检）统一纳入 proactive 管理
- `oc-cli` 命令树：serve/doctor/onboard/agent/models/config/status/cron/tasks/memory
- 验收：**"周五提醒我"能触发并主动推送**；heartbeat 驱动 dreaming + 卡死扫描（复用 M3 tick）；每日总结类周期任务可跑；anti-nagging 限额生效

到 M6 = 一个能对话、能生成并(经审批)执行脚本、记得住你、会主动提醒、复杂任务不卡死的**真正个人助手**。

---

## 10. Phase 2+（明确后置）

沙箱(docker)、更多协议适配器(Telegram/Discord/微信…)、WS 远程 + 鉴权、媒体(图/音/视频/TTS/转写)、Talk 实时语音、Control UI/Canvas、插件系统(若真需要)、OTel/Prometheus、多 provider 扩展。

---

## 11. 与 03 版的关系

- 03 = 精简通用网关（多渠道/多 provider/插件为潜在方向，沙箱建议提前）。
- **04（本版）= 专用个人助手（权威）**：砍生态、单用户、协议+CLI 交互、记忆与主动性进 MVP、沙箱诚实后置。
- 两版共享同一套核心机制（[02-核心机制.md](02-核心机制.md)）与技术栈判断（rusqlite 单写线程 / ArcSwap 代际快照 / tokio 精简 feature）。

---

## 12. 一句话总结

用 **单用户假设**砍掉 OpenClaw 的整个生态层，把复杂度预算全投到**记忆(含 Dreaming/跨会话/意图)+ 主动性(cron/heartbeat/提醒)+ 防卡死**三件事上；交互收敛为**本地 socket + 干净协议 + TUI**（渠道以后当适配器）；技术栈保持 **tokio(精简)/rusqlite(单写线程)/reqwest(rustls)/ArcSwap(代际快照)** 的最小运行时。走完 M1–M6，得到一个小、快、记得住你、会主动找你、且复杂任务不卡死的真正个人助手。

