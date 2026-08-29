# oc

一个用 Rust 写的**个人助手 daemon**。常驻后台、能自己动（定时/主动提醒）、会记事（分层记忆 + 睡眠巩固），通过终端 UI 与你对话，接 DeepSeek 等 OpenAI 兼容模型。

设计以「纯核心 + 单向依赖 + 单写库」为骨架，把所有决策收进可确定性测试的领域层，把 IO / 并发 / 时钟隔离在外围。当前 110 个单元测试全绿。

> 状态：早期骨架（M1–M6 已落地主干闭环）。能跑，但仍有明确缺口，见 [docs/03-实现状态与缺口.md](docs/03-实现状态与缺口.md)。

---

## 特性

**对话与编排**
- Agent 循环由纯状态机（`oc-core`）驱动，模型传输走统一 `Delta` 流，可流式显示。
- 会话持久化：重启不失忆，重连即恢复上下文。
- Prompt 组装确定性字节稳定（稳定前缀利于缓存 + 易变时间后缀），工具/技能/记忆按稳定键排序注入。
- SOUL.md 人格 + USER.md / AGENTS.md / MEMORY.md 分层上下文加载。

**记忆**
- 分层记忆（episodic / curated），Lane1 词法检索：相关度 × 30 天半衰期 × 重要度。
- 显式记忆捕获：识别「记住…」「remember …」等触发词，落库为 curated + Owner 来源。
- Dreaming 睡眠巩固：两道门（确定性打分/频次/时窗 + 结构性排除 Untrusted/System 来源）挑选 episodic 升级为 curated，全程写审计。

**主动性**
- 定时任务：自写 5 字段 cron 解析器（`分 时 日 月 周`，支持 `*` `*/n` `a-b` `a,b,c`），心跳到点触发、执行提示词、发提醒、自动续期。
- 防打扰策略：冷却 24h / 预算 3 次 / 90 天过期（顺序 过期→预算→冷却）。

**工具**
- exec / file / process 三类本地工具 + 一次性通知消息工具。
- Web 工具（`web` feature）：WebFetch（正文抽取）+ WebSearch（DuckDuckGo HTML 端点，无需 API key）。
- 交互式审批门：敏感操作可要求确认（`prompt | allow | deny`）。

**可靠性与安全**
- 单库 SQLite（WAL + 单写线程 + 前向迁移 + `user_version`）。
- 审计哈希链（FNV-1a），来源分级 provenance（绝不默认 Owner）。
- 单实例锁防止多个 daemon 争用同一 socket / 库。
- 空闲看门狗 + 心跳诊断 + 后台任务台账。

---

## 架构

单向、无环依赖。核心是纯的，外围才碰 IO。

```
oc-cli ──▶ oc-tui ──▶ oc-server ──▶ oc-core   (纯策略：不 spawn / 不连接 / 不读时钟 / 不 rand)
                          │      └─▶ oc-store  (单库 SQLite，单写线程)
                          │      └─▶ oc-tools  (工具 trait + 审批门 + 结果净化)
                          └────────▶ oc-llm    (Provider trait + 统一 Delta 流)
                     oc-proto  (CLI/TUI ↔ daemon 线上契约，纯类型)
```

| crate | 职责 |
|---|---|
| `oc-proto` | 交互协议 DTO，纯类型 + serde，无逻辑无 IO |
| `oc-core` | 领域层，纯策略集合，100% 确定性可测 |
| `oc-store` | 单库存储，schema 版本化 + WAL + 单写线程 |
| `oc-llm` | 模型传输，Provider trait + 统一 Delta 流 |
| `oc-tools` | 工具集，Tool trait + 策略管道 + exec 审批门 |
| `oc-server` | 常驻进程，agent 循环 / 看门狗 / 心跳 / 主动性 |
| `oc-tui` | 本地终端 UI，协议第一个 client，纯展示 |
| `oc-cli` | CLI 入口，doctor / serve / onboard / cron / memory / status |

细节见 [docs/02-详细设计文档.md](docs/02-详细设计文档.md)。

---

## 使用指南

### 前置

- Rust 1.90+
- 一个 OpenAI 兼容模型的 API key（默认示例用 DeepSeek）

### 构建

```bash
cargo build --release
```

### 初始化

生成 `~/.oc` 骨架（`config.toml` + `soul/*.md` + `skills/` + `memory/` + `logs/`）：

```bash
oc onboard
```

配置 API key（推荐环境变量，见 [config.example.toml](config.example.toml)）：

```bash
export DEEPSEEK_API_KEY=sk-xxxx        # Windows: setx DEEPSEEK_API_KEY sk-xxxx
```

校验环境与配置（建库 / 迁移 / schema 检查）：

```bash
oc doctor
oc doctor --dump-schema
```

### 启动与对话

```bash
oc serve      # 启动常驻进程（阻塞）
oc            # 无子命令 → 连上 daemon 进 TUI 对话
```

### 定时任务（主动性）

```bash
oc cron add "0 9 * * 1-5" "早上好，汇总今天日程"   # 工作日 9 点触发
oc cron list
oc cron rm <cron_id>
```

### 记忆检索（自省/调试）

```bash
oc memory search "上次讨论的架构决策" --limit 5
```

### 状态

```bash
oc status     # daemon 状态快照
```

### 配置要点

`~/.oc/config.toml`（示例见 [config.example.toml](config.example.toml)）：

- `[server] transport` — `pipe`（Windows）/ `unix`（Linux/macOS）/ `ws`
- `[[models]]` — DeepSeek 用 `provider = "openai"` + `base_url = "https://api.deepseek.com/v1"`
- `[memory]` — 半衰期 / 触发阈值；`vec = false`（向量检索尚未默认开启）
- `[proactive]` — 心跳周期 + 防打扰预算
- `[tools.approval] mode` — 审批门策略

---

## 后续迭代方向与计划

按优先级，详细清单与状态见 [docs/03-实现状态与缺口.md](docs/03-实现状态与缺口.md)。

**近期（收口主动性与记忆闭环）**
- Standing intent 触发链接线：纯策略已就绪，补 store ops + session Submit 钩子。
- Dreaming 重写 MEMORY.md：当前仅升级 DB 层级，让睡眠巩固名副其实。
- USER.md supersede 接线：让新事实覆盖旧事实。
- `ask_user` 工具：需在协议层加自由文本回信通道 + TUI 输入态（目前只有 yes/no 审批）。

**中期（配置与可靠性）**
- ArcSwap 代际快照 / 配置热更新（当前仅注释）。
- Provider failover 接线（`failover_plan` / `resolve_alias` 已实现但无调用方）。
- 双平台 CI 矩阵（unix socket 目前只编译不在 Linux 跑）。
- 抽出 `oc-core::context` 模块（上下文加载现散落在 server::session）。

**远期（能力增强）**
- 向量语义检索（`sqlite-vec` feature，接入 embedding 后开启）。
- 修正 cron 的 day-of-month / day-of-week 语义（当前用 AND，标准应为 OR）。

---

## 开发

### 开发期启动（未安装到 PATH）

上面「使用指南」里的 `oc` 是已安装的二进制。开发时用 `cargo run -p oc-cli --` 代替，子命令原样跟在 `--` 后面。

首次准备（生成 `~/.oc` 骨架 + 建库，只需一次）：

```bash
cargo run -p oc-cli -- onboard
cargo run -p oc-cli -- doctor
export DEEPSEEK_API_KEY=sk-xxxx        # Windows: setx DEEPSEEK_API_KEY sk-xxxx
```

两个终端跑起来对话：

```bash
# 终端 1：启动 daemon（阻塞；单实例锁，同时只能开一个）
cargo run -p oc-cli -- serve

# 终端 2：连上 daemon 进 TUI
cargo run -p oc-cli
```

其余子命令同理：

```bash
cargo run -p oc-cli -- cron list
cargo run -p oc-cli -- memory search "关键词" --limit 5
cargo run -p oc-cli -- status
```

想看 daemon 细节日志：

```bash
RUST_LOG=oc_server=debug cargo run -p oc-cli -- serve
```

要点：先 `onboard` 再 `serve`（serve 会读 `~/.oc/config.toml`，缺失会失败）；跑 TUI 的终端不要再 `serve`（单实例锁）；首次 `cargo run` 会编译整个 workspace，之后增量很快。

### 测试与检查

```bash
cargo test               # 全部测试（当前 113 个）
cargo test -p oc-core    # 单 crate
cargo clippy --all-targets
```

`oc-core` 是纯的——不 spawn、不开连接、不读时钟、不 rand，所有外部量作为参数传入、返回决策，因此可完全确定性测试。改核心逻辑时保持这一约束。

## 许可

MIT OR Apache-2.0
