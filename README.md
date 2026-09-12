# oc

一个用 Rust 写的**个人助手 daemon**。常驻后台、能自己动（定时/主动提醒）、会记事（分层记忆 + 睡眠巩固），通过终端 UI 与你对话，接 DeepSeek / Anthropic 等 OpenAI 兼容模型。

设计以「纯核心 + 单向依赖 + 单写库」为骨架，把所有决策收进可确定性测试的领域层，把 IO / 并发 / 时钟隔离在外围。

```bash
oc onboard && oc doctor && oc serve   # 初始化并启动 daemon
oc                                     # 另开终端，进 TUI 对话
```

完整步骤见 [快速开始](docs/guides/quickstart.md)。

---

## 特性

**对话与编排**
- Agent 循环由纯状态机（`oc-core`）驱动，模型传输走统一 `Delta` 流，可流式显示。
- 会话持久化：重启不失忆，重连即恢复上下文。
- Prompt 组装确定性字节稳定（稳定前缀利于缓存 + 易变时间后缀），工具/技能/记忆按稳定键排序注入。
- SOUL.md 人格 + USER.md / AGENTS.md / MEMORY.md 分层上下文加载。
- 斜杠指令（`/new` `/clear` `/compact` `/stop` `/status` 等 14 条）下沉 daemon 统一解析，TUI 与 Web UI 共享一套指令集。

**记忆**
- 分层记忆（episodic / curated），Lane1 词法检索：相关度 × 30 天半衰期 × 重要度。
- 显式记忆捕获：识别「记住…」「remember …」等触发词，落库为 curated + Owner 来源。
- 偏好 supersede：新事实就地替换同主题旧记忆，而非并列矛盾两条。
- Dreaming 睡眠巩固：两道门（确定性打分/频次/时窗 + 结构性排除 Untrusted/System 来源）挑选 episodic 升级为 curated，全程写审计。
- FTS5 全文索引：10 万条量级检索 1–7ms，中文靠 2-gram 预切词。

**主动性**
- 定时任务：自写 5 字段 cron 解析器（`分 时 日 月 周`，支持 `*` `*/n` `a-b` `a,b,c`），**按 IANA 时区解释**（本机时区默认，可显式指定），心跳到点触发、执行提示词、发提醒、自动续期。
- 一次性延时：`delay` 支持「N 秒后提醒」（cron 表达式最小粒度是分钟，故另走精确 timer，秒级准）。
- 模型可自行管理提醒：`cron` 工具 `add/delay/list/rm`，到点由系统主动推送、不占用对话。
- 话题触发式待办（standing intent）：聊到指定关键词时注入提醒。
- 防打扰策略：冷却 24h / 预算 3 次 / 90 天过期（顺序 过期→预算→冷却）。

**工具**
- exec / file / process 三类本地工具 + ask_user 主动提问 + 一次性通知消息工具。
- Web 工具（`web` feature）：WebFetch（正文抽取）+ WebSearch（DuckDuckGo HTML 端点，无需 API key）。
- 交互式审批门：敏感操作可要求确认（`prompt | allow | deny`）。

**OpenAI API 兼容**
- `oc http` 提供 OpenAI Responses API 兼容端点（`POST /v1/responses`），可直接对接 OpenAI SDK / LangChain。
- 支持流式 SSE 与非流式、动态 `instructions`、`user` / `previous_response_id` 会话延续、base64 文件输入。
- 独立进程：HTTP 层崩溃不影响核心会话。接口与边界见 [protocol.md](docs/reference/protocol.md)。

**可靠性与安全**
- 单库 SQLite（WAL + 单写线程 + 独立读连接池 + 前向迁移 + `user_version`）。
- 写线程 panic 后读路径仍可用，降级而非整体瘫痪。
- 审计哈希链（FNV-1a），来源分级 provenance（绝不默认 Owner）。
- 单实例锁防止多个 daemon 争用同一 socket / 库。
- 空闲看门狗 + 心跳诊断 + 后台任务台账 + panic 隔离（单 run 崩溃不杀进程）。

---

## 架构

单向、无环依赖。核心是纯的，外围才碰 IO。

```
oc-cli ──▶ oc-tui  ──▶ oc-server ──▶ oc-core   (纯策略：不 spawn / 不连接 / 不读时钟 / 不 rand)
   │                      │      └─▶ oc-store  (单库 SQLite，单写线程 + 读池)
   │                      │      └─▶ oc-tools  (工具 trait + 审批门 + 结果净化)
   │                      └────────▶ oc-llm    (Provider trait + 统一 Delta 流)
   └─────▶ oc-http ─ ─ ─▶ (经 socket/pipe 连到独立进程的 oc-server)
                     oc-proto  (client ↔ daemon 线上契约，纯类型)
```

| crate | 职责 |
|---|---|
| `oc-proto` | 交互协议 DTO，纯类型 + serde，无逻辑无 IO |
| `oc-core` | 领域层，纯策略集合，100% 确定性可测 |
| `oc-store` | 单库存储，schema 版本化 + WAL + 单写线程 + FTS5 |
| `oc-llm` | 模型传输，Provider trait + 统一 Delta 流 |
| `oc-tools` | 工具集，Tool trait + 策略管道 + exec 审批门 |
| `oc-server` | 常驻进程，agent 循环 / 看门狗 / 心跳 / 主动性 |
| `oc-tui` | 本地终端 UI，协议第一个 client，纯展示 |
| `oc-http` | OpenAI Responses API 兼容层，协议适配 + SSE（独立进程） |
| `oc-cli` | CLI 入口，doctor / serve / http / onboard / cron / intent / memory / status / debug |

详见 [架构概览](docs/architecture/overview.md)。

---

## 文档

| 我想… | 看这里 |
|---|---|
| 装上并跑起来 | [快速开始](docs/guides/quickstart.md) · [安装](docs/operations/install.md) |
| 知道能干什么、怎么用 | [使用指南](docs/guides/usage.md) |
| 查某个命令或配置项 | [CLI 参考](docs/reference/cli.md) · [配置参考](docs/reference/config.md) |
| 出问题了 | [故障排查](docs/operations/troubleshooting.md) |
| 理解内部怎么运作 | [架构文档](docs/architecture/overview.md) |
| 看改了什么 / 接下来做什么 | [CHANGELOG](CHANGELOG.md) · [看板](BOARD.md) |

文档总索引：[docs/README.md](docs/README.md)。

---

## 开发

```bash
cargo build --workspace
cargo test --workspace        # 全量测试
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/e2e/smoke.sh     # 进程级冒烟（真二进制 + 真 CLI + 真 HTTP）
```

开发期启动（未安装到 PATH）：

```bash
# 终端 1：启动 daemon（阻塞；单实例锁，同时只能开一个）
cargo run --bin oc -- serve

# 终端 2：连上 daemon 进 TUI
cargo run --bin oc
```

调试：`RUST_LOG=oc_server=debug cargo run --bin oc -- serve`，日志落在 `~/.oc/logs/`。运行时状态用 `oc debug --watch`。

测试策略与覆盖矩阵见 [development/testing.md](docs/development/testing.md)，发布流程见 [development/release.md](docs/development/release.md)。

---

## 许可

MIT OR Apache-2.0
