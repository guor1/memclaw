# CLI 参考

二进制名称：`oc`

---

## oc

```
oc
```

无子命令时，连接已在运行的 daemon，启动 TUI 对话界面。

需要先运行 `oc serve` 启动 daemon。

---

## oc doctor

```
oc doctor [--dump-schema]
```

建库/迁移检查、配置校验。首次使用或更新后运行，确认环境就绪。

| 参数 | 说明 |
|---|---|
| `--dump-schema` | 把协议 JSON Schema 导出到 `schema/oc-proto.json` |

检查项：
- `~/.oc/` 目录及数据库是否存在，schema 版本是否匹配
- 表结构是否与当前代码一致（`check_shape`，开发阶段改 DDL 不写迁移时可检出差异）
- `config.toml` 是否通过校验
- `~/.oc/workspace` 工作区是否存在

---

## oc serve

```
oc serve [--socket <路径>]
```

启动常驻进程（前台阻塞）。Ctrl-C 停止。

| 参数 | 说明 |
|---|---|
| `--socket <路径>` | 覆盖监听的 socket 路径或 Windows 管道名。主要供测试用——Windows 默认管道名是全局常量，仅靠 `OC_HOME` 无法隔离测试 daemon 与开发机上正在运行的实例 |

日志同时写到 `~/.oc/logs/oc.log.YYYY-MM-DD`（按天滚动）和 stderr。

---

## oc http

```
oc http [--port <端口>] [--socket <路径>] [--max-conns <数量>]
```

启动 OpenAI Responses API 兼容的 HTTP 网关，绑定 `127.0.0.1`，无鉴权。只做协议适配，实际会话由已在运行的 daemon 处理——两个进程分开，HTTP 侧崩溃不影响核心会话。

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--port` | `8080` | HTTP 监听端口 |
| `--socket` | 平台默认值 | 连接 daemon 用的 socket/管道路径 |
| `--max-conns` | `32` | 到 daemon 的最大并发连接数，超出后新请求等待 10s 再返回 503 |

**注意**：该端点等同于对 daemon 的完全访问权。需要远程访问时应在前面放带认证的反向代理，不要直接暴露到网络。

---

## oc onboard

```
oc onboard
```

交互式初始化，引导生成 `~/.oc/` 目录骨架，包含 `config.toml`、`soul/SOUL.md`、`soul/USER.md`、`soul/AGENTS.md`、`soul/MEMORY.md`、`skills/example.md`。

---

## oc status

```
oc status
```

打印当前会话状态快照，包含：活跃 run id、队列深度、后台任务数、上下文用量（已用/总量/百分比）、当前模型和 provider。

---

## oc sessions

```
oc sessions
```

列出所有会话（id、kind、创建时间）。

---

## oc compact

```
oc compact
```

请求压缩当前会话的上下文，把旧历史摘要化，释放上下文窗口空间。

---

## oc debug

```
oc debug [--watch]
```

打印诊断快照：uptime、写线程状态、订阅数、幂等表条目数，以及每个会话的队列深度、当前 phase、活跃 run id、运行时长、距上次进展时长、工具轮数、最近错误。

| 参数 | 说明 |
|---|---|
| `--watch` | 每秒刷新一次，Ctrl-C 退出。用于观察「不回复 / 卡住」时状态如何演变 |

`phase` 可能值：`starting`、`awaiting-mdl`、`streaming`、`tool-exec`、`compacting`、`idle`。

`writer OK / DOWN`：`DOWN` 表示写线程已停止，读路径仍可用，但写操作会返回错误，需重启 daemon。

---

## oc cron

定时任务管理（主动性功能）。到点触发时，daemon 会以对应提示词发起一次新 run。

### oc cron add

```
oc cron add <EXPR> <PROMPT> [--tz <时区>]
```

| 参数 | 说明 |
|---|---|
| `EXPR` | 5 字段 cron 表达式（分 时 日 月 周），如 `"0 9 * * 1-5"` |
| `PROMPT` | 触发时执行的提示词 |
| `--tz` | IANA 时区名，如 `Asia/Shanghai`。省略则用本机时区。表达式按此时区的本地时间解释，拼错的时区名会报错（不会静默回退到 UTC） |

### oc cron list

```
oc cron list
```

列出所有定时任务，包含 id、表达式、下次触发时间（本地时区）、剩余时长、提示词。

### oc cron rm

```
oc cron rm <CRON_ID>
```

按 id 删除定时任务。id 从 `oc cron list` 获取。

---

## oc intent

话题触发式待办管理（standing intent，主动性功能）。与 cron 的区别：cron 到点触发，intent 由对话话题命中关键词触发。

### oc intent add

```
oc intent add <TEXT> <KEYWORDS...> [--cooldown-secs N] [--budget N] [--expiry-days N]
```

| 参数 | 说明 |
|---|---|
| `TEXT` | 触发时注入的提醒正文，如 `"带转换插头"` |
| `KEYWORDS...` | 触发关键词，可给多个，命中任一即触发，如 `出差 德国` |
| `--cooldown-secs` | 同一条待办两次提醒的最小间隔（秒），省略取 `[proactive]` 配置默认值 |
| `--budget` | 最多提醒次数，用尽即静默，省略取配置默认值 |
| `--expiry-days` | 多少天后过期，0 = 不过期，省略取配置默认值 |

### oc intent list

```
oc intent list
```

列出所有话题待办，含已触发次数、剩余次数、关键词、上次触发时间。

### oc intent rm

```
oc intent rm <INTENT_ID>
```

按 id 删除话题待办。id 从 `oc intent list` 获取。

---

## oc memory

记忆检索，主要用于调试和自省。

### oc memory search

```
oc memory search <QUERY> [--limit N]
```

| 参数 | 说明 |
|---|---|
| `QUERY` | 查询词，走 FTS5 词法检索 |
| `--limit` | 返回条数上限 |

输出格式：`[score] (tier) text`。

---

## 环境变量

| 变量 | 说明 |
|---|---|
| `OC_HOME` | 覆盖 `~/.oc/` 根目录。`oc doctor`、`oc serve` 和所有客户端命令均尊重此变量 |
| `OC_SOCKET` | 覆盖 socket 路径或 Windows 管道名。daemon 和所有 CLI/TUI 客户端均尊重此变量，用于测试端点隔离 |
| `RUST_LOG` | 日志级别过滤。`oc serve` 默认 `oc=info,oc_server=info,oc_llm=info`；`oc http` 默认 `oc=info,oc_http=info` |
| `ANTHROPIC_API_KEY` | Anthropic API key，在 `config.toml` 里用 `{env="ANTHROPIC_API_KEY"}` 引用 |
| `DEEPSEEK_API_KEY` | DeepSeek API key，在 `config.toml` 里用 `{env="DEEPSEEK_API_KEY"}` 引用 |
