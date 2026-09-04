# oc

一个用 Rust 写的**个人助手 daemon**。常驻后台、能自己动（定时/主动提醒）、会记事（分层记忆 + 睡眠巩固），通过终端 UI 与你对话，接 DeepSeek 等 OpenAI 兼容模型。

设计以「纯核心 + 单向依赖 + 单写库」为骨架，把所有决策收进可确定性测试的领域层，把 IO / 并发 / 时钟隔离在外围。当前 242 个测试全绿。

> 状态：早期骨架（M1–M6 主干闭环 + P0/P1-1~P1-5 已落地）。能跑，但仍有明确缺口，
> 见 [docs/plan/下一阶段计划.md](docs/plan/下一阶段计划.md)。文档总索引：[docs/README.md](docs/README.md)。

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
- 定时任务：自写 5 字段 cron 解析器（`分 时 日 月 周`，支持 `*` `*/n` `a-b` `a,b,c`），**按 IANA 时区解释**（本机时区默认，可显式指定），心跳到点触发、执行提示词、发提醒、自动续期。
- 一次性延时：`delay` 支持「N 秒后提醒」（cron 表达式最小粒度是分钟，故另走精确 timer，秒级准）。
- 模型可自行管理提醒：`cron` 工具 `add/delay/list/rm`，到点由系统主动推送、不占用对话。
- 防打扰策略：冷却 24h / 预算 3 次 / 90 天过期（顺序 过期→预算→冷却）。

**工具**
- exec / file / process 三类本地工具 + 一次性通知消息工具。
- Web 工具（`web` feature）：WebFetch（正文抽取）+ WebSearch（DuckDuckGo HTML 端点，无需 API key）。
- 交互式审批门：敏感操作可要求确认（`prompt | allow | deny`）。

**OpenAI API 兼容**
- `oc http` 提供 OpenAI Responses API 兼容端点（`POST /v1/responses`），可直接对接 OpenAI SDK / LangChain。
- 支持流式 SSE 与非流式、动态 `instructions`、`user` / `previous_response_id` 会话延续、base64 文件输入。
- 独立进程：HTTP 层崩溃不影响核心会话。覆盖主流文本对话用例，边界见 [方案文档](docs/design/OpenAI-Responses-API-方案.md)。

**可靠性与安全**
- 单库 SQLite（WAL + 单写线程 + 前向迁移 + `user_version`）。
- 审计哈希链（FNV-1a），来源分级 provenance（绝不默认 Owner）。
- 单实例锁防止多个 daemon 争用同一 socket / 库。
- 空闲看门狗 + 心跳诊断 + 后台任务台账。

---

## 架构

单向、无环依赖。核心是纯的，外围才碰 IO。

```
oc-cli ──▶ oc-tui  ──▶ oc-server ──▶ oc-core   (纯策略：不 spawn / 不连接 / 不读时钟 / 不 rand)
   │                      │      └─▶ oc-store  (单库 SQLite，单写线程)
   │                      │      └─▶ oc-tools  (工具 trait + 审批门 + 结果净化)
   │                      └────────▶ oc-llm    (Provider trait + 统一 Delta 流)
   └─────▶ oc-http ─ ─ ─▶ (经 socket/pipe 连到独立进程的 oc-server)
                     oc-proto  (client ↔ daemon 线上契约，纯类型)
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
| `oc-http` | OpenAI Responses API 兼容层，协议适配 + SSE（独立进程） |
| `oc-cli` | CLI 入口，doctor / serve / http / onboard / cron / memory / status / debug |

细节见 [docs/design/04-详细设计文档.md](docs/design/04-详细设计文档.md)。

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

### OpenAI API 兼容（HTTP 网关）

启动 HTTP 网关（需先启动 daemon）：

```bash
oc http                    # 默认端口 8080，监听 127.0.0.1
oc http --port 3000        # 自定义端口
```

**curl 测试**：

```bash
# 非流式
curl -X POST http://127.0.0.1:8080/v1/responses \
  -H "Content-Type: application/json" \
  -d '{"input": "What is 2+2?", "stream": false}'

# 流式
curl -N -X POST http://127.0.0.1:8080/v1/responses \
  -H "Content-Type: application/json" \
  -d '{"input": "Count to 5", "stream": true}'
```

**OpenAI SDK 集成**（Python）：只实现了 Responses API，用 `client.responses`，不是 `client.chat.completions`（后者是另一个端点，未实现）。

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8080/v1",
    api_key="dummy",  # 当前无鉴权，任意非空值
)

resp = client.responses.create(input="Hello!")
print(resp.output[0].content[0].text)

# 流式
with client.responses.create(input="Count to 5", stream=True) as stream:
    for event in stream:
        if event.type == "response.output_text.delta":
            print(event.delta, end="", flush=True)
```

**会话延续**：传同一个 `user` 会派生出稳定会话，后续请求自动带上下文；也可以用上一次的响应 id。

```python
a = client.responses.create(input="My favorite color is blue", user="alice")
b = client.responses.create(input="What is my favorite color?", user="alice")
print(b.output[0].content[0].text)  # 提到 blue

# 或显式接续某个响应
c = client.responses.create(input="And my second favorite?",
                            previous_response_id=a.id)
```

`model` 参数会被忽略——实际用哪个模型由 daemon 的 `~/.oc/config.toml` 决定。完整参数支持情况、
不支持的特性（多模态、动态 `tools`、`background`）见
[OpenAI-Responses-API-方案.md](docs/design/OpenAI-Responses-API-方案.md)。

### 记忆检索（自省/调试）

```bash
oc memory search "上次讨论的架构决策" --limit 5
```

### 状态与诊断

```bash
oc status            # daemon 状态快照（会话/活跃 run/上下文用量）
oc debug             # 运行时诊断快照：各会话 phase、队列深度、车道占用、写线程健康
oc debug --watch     # 每秒刷新，观察「不回复 / 卡住」时状态如何演变（Ctrl-C 退出）
```

排查「消息不回复 / 回复截断 / 卡死」这类时序问题时，用 `oc debug --watch` 实时看 run 卡在哪个阶段
（`awaiting-mdl` 长时间无 `last_delta` 即模型侧无响应），配合 daemon 日志（见下「开发」）定位。

### 配置要点

`~/.oc/config.toml`（示例见 [config.example.toml](config.example.toml)）：

- `[server] transport` — `pipe`（Windows）/ `unix`（Linux/macOS）/ `ws`
- `[[models]]` — DeepSeek 用 `provider = "openai"` + `base_url = "https://api.deepseek.com/v1"`
- `[memory]` — 半衰期 / 触发阈值；`vec = false`（向量检索尚未默认开启）
- `[proactive]` — 心跳周期 + 防打扰预算
- `[tools.approval] mode` — 审批门策略

---

## 后续迭代方向与计划

按优先级，详细清单与状态见 [docs/plan/下一阶段计划.md](docs/plan/下一阶段计划.md)。

**近期（收口主动性与记忆闭环）**
- 真机复验 P1-2~P1-6：六项 P1 至今只有自动化覆盖，历史上 P0-1 / P1-1 的缺陷都是真机才暴露。
  P1-6 尤其需要：巩固要等 episodic 候选攒够 use_count 且沉淀 ≥3 天，只有挂着跑才看得到。
- `oc http` 真机端到端：起 daemon + `oc http`，用真实 OpenAI SDK 打通。

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

### 调试：日志 + 诊断快照

daemon 与 TUI 是两个进程，`warn!`/错误都打在**跑 `serve` 的那个终端**。日志同时落盘到
`~/.oc/logs/oc.log.YYYY-MM-DD`（按天滚动），排障时可直接 tail：

```bash
# 终端 1：以 debug 级日志启动 daemon（含每个 run 的 span、建流延迟、finish_reason、看门狗等）
RUST_LOG=oc=debug cargo run -p oc-cli -- serve

# 另开一个终端：跟踪落盘日志（Windows 用 Get-Content -Wait 或编辑器打开）
tail -f ~/.oc/logs/oc.log.*
```

`RUST_LOG` 过滤器可按 crate/级别细调，例如 `oc=debug,oc_llm=info`（默认 `oc=info,oc_server=info,oc_llm=info`）。

实时看运行时状态（不用翻日志）：

```bash
cargo run -p oc-cli -- debug            # 打印一次诊断快照
cargo run -p oc-cli -- debug --watch    # 每秒刷新
```

一次典型的「不回复」排查：一边 `oc debug --watch` 看 run 卡在哪个 phase，一边 `tail -f` 看
run span 是否起步、卡在哪个 await、`finish_reason` 是什么。

要点：先 `onboard` 再 `serve`（serve 会读 `~/.oc/config.toml`，缺失会失败）；跑 TUI 的终端不要再 `serve`（单实例锁）；首次 `cargo run` 会编译整个 workspace，之后增量很快。

### 测试与检查

```bash
cargo test               # 全部测试（当前 242 个）
cargo test -p oc-core    # 单 crate
cargo clippy --all-targets
```

`oc-core` 是纯的——不 spawn、不开连接、不读时钟、不 rand，所有外部量作为参数传入、返回决策，因此可完全确定性测试。改核心逻辑时保持这一约束。

## 许可

MIT OR Apache-2.0
