# 配置参考

配置文件路径：
- Windows：`C:\Users\<你>\.oc\config.toml`
- Linux/macOS：`~/.oc/config.toml`

可用 `OC_HOME` 环境变量覆盖根目录。运行 `oc doctor` 校验配置是否有效。

**所有字段修改后均需重启 `oc serve` 才生效**，不支持热更新。

---

## [server]

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `transport` | string | Linux/macOS: `"unix"` / Windows: `"pipe"` | 传输方式。`unix` = AF_UNIX domain socket；`pipe` = Windows 命名管道 |

---

## [[models]]

模型配置数组，至少需要一条。多条时通过 `alias` 区分。

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `alias` | string | `"default"` | 模型别名，用于在多模型配置中区分 |
| `provider` | string | — | 必填。`"openai"` 或 `"anthropic"`。DeepSeek 等 OpenAI 兼容接口填 `"openai"` |
| `model` | string | — | 必填。模型名，如 `"deepseek-v4-flash"`、`"claude-opus-5"`。也用于查内置 context window 默认值 |
| `hosting` | string | `"cloud"` | `"cloud"` = 空闲看门狗 120s；`"self"` = 自托管模型，看门狗宽松为 300s |
| `api_key` | 引用对象 | — | 必填。三种格式之一：`{env="VAR"}`（推荐）/ `{file="path/to/key"}` / `{inline="sk-…"}`（不建议明文） |
| `base_url` | string | 按 provider 官方地址 | 自定义 API 基地址。使用 DeepSeek 时填 `"https://api.deepseek.com"` |
| `context_window` | u32 | 按 model 名查内置表 | 上下文窗口 token 数，压缩预算由此派生。内置默认值：claude=200000、deepseek=65536、gpt-4o=128000、gemini=1000000、未知=32768 |
| `max_output_tokens` | u32 | `min(context_window, 8192)` | 单轮输出上限（请求体 `max_tokens`/`max_completion_tokens`）。省略时自动计算，一般不用手填。若日志出现 `WARN 模型输出被 max_tokens 截断` 则按需调大 |
| `max_tokens_field` | string | 按 provider 自动判断 | 强制指定请求体字段名：`"max_tokens"` 或 `"max_completion_tokens"`。换新端点若输出总被截断可用此项排查 |

---

## [memory]

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `vec` | bool | `false` | 向量语义检索开关。sqlite-vec feature 启用前保持 `false` |
| `halflife_days` | u32（≥1） | `30` | 记忆半衰期（天），影响排名衰减速度 |
| `trigger_threshold` | f32（0.0–1.0） | `0.72` | 记忆注入的相关性阈值，低于此值不注入 |
| `trigger_max_per_turn` | u32（≥1） | `3` | 每轮最多注入的记忆条数 |

---

## [proactive]

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `heartbeat_secs` | u64（≥1） | `60` | 心跳间隔（秒）。驱动 GC、卡死诊断扫描、cron 到期扫描 |
| `intent_cooldown_secs` | u64 | `86400` | 话题待办（standing intent）两次提醒的最小间隔（秒），新建待办时写入，可被 `oc intent add --cooldown-secs` 逐条覆盖 |
| `intent_budget` | u32（≥1） | `3` | 同一条待办最多提醒次数，用尽即静默 |
| `intent_expiry_days` | u32 | `90` | 待办过期天数；`0` = 永不过期 |
| `intent_max_per_turn` | u32（≥1） | `3` | 每轮最多注入的待办条数，防止一条消息命中多条待办时塞满上下文 |

---

## [tools]

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `exec_timeout_secs` | u64（≥1） | `120` | 单个工具调用的执行超时（秒） |

---

## [tools.approval]

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `mode` | string | `"prompt"` | 工具审批模式。`"prompt"` = 危险操作请求用户审批；`"allow"` = 全放行；`"deny"` = 全拒绝 |
| `timeout_secs` | u64 | `120` | 等待审批回执的上限（秒），超时按拒绝处理。`0` = 不超时。**无人值守场景（cron / oc http）不能设为 0**，否则该轮永久占用会话车道，直到卡死诊断兜底（约 360s） |

---

## [watchdog]

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `idle_cloud_secs` | u64（≥1） | `120` | `hosting="cloud"` 模型的空闲超时（秒）。超过此时长无 token 输出则判为卡死候选 |
| `idle_self_secs` | u64（≥1） | `300` | `hosting="self"` 模型的空闲超时（秒） |
| `run_timeout_secs` | u64 | `0` | 单次 run 的墙钟上限（秒）。`0` = 不超时 |
| `abort_min_secs` | u64（≥1） | `300` | 卡死 abort 的最小空闲时长（秒）。需同时满足空闲时长 ≥ `abort_min_secs` 且 ≥ 3 倍警告阈值时才触发 abort，避免误杀长时间调工具的 run |

---

## 最小配置示例

```toml
proto_version = 1

[server]
transport = "pipe"   # Windows 用 pipe，Linux/macOS 用 unix

[[models]]
alias = "default"
provider = "openai"
model = "deepseek-v4-flash"
hosting = "cloud"
base_url = "https://api.deepseek.com"
api_key = { env = "DEEPSEEK_API_KEY" }

[memory]
vec = false
halflife_days = 30
trigger_threshold = 0.72
trigger_max_per_turn = 3

[proactive]
heartbeat_secs = 60
intent_cooldown_secs = 86400
intent_budget = 3
intent_expiry_days = 90
intent_max_per_turn = 3

[tools]
exec_timeout_secs = 120
[tools.approval]
mode = "prompt"
timeout_secs = 120

[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
```
