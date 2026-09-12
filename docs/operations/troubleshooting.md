# 故障排查

## 可观测性工具

```bash
oc status          # 会话状态快照（活跃 run / 排队数 / 上下文用量）
oc debug           # 诊断快照（uptime / writer 状态 / 每会话 phase）
oc debug --watch   # 每秒刷新，观察卡住时状态演变（Ctrl-C 退出）
oc doctor          # 配置校验 + 数据库检查
```

日志位置：`~/.oc/logs/oc.log.YYYY-MM-DD`。调高日志级别：

```bash
RUST_LOG=oc_server=debug oc serve
```

### 日志分层

默认级别 `oc=info,oc_server=info,oc_llm=info`，只记录会话级事件：每条日志一行，
按 `submit 受理` → `run 起步` → `close` → `run 完成/非正常终态` 的顺序铺开一轮。

- **`run 完成` / `run 非正常终态`** 是单轮的聚合汇总行：带 `run_id`、`outcome`
  （非正常时）、`tool_rounds`（这一轮模型实际调了几次工具）、`ms`（墙钟耗时）。
  想快速回看「某轮到底干了啥」，先按 `run_id` 把这两行抓出来。
- **`submit 受理` 的 `chars`** 是用户输入长度，`close` 的 `time.busy`/`time.idle`
  是这一轮里连接在忙/空闲的累计时长。

更高层级打开后才有细节，按需叠加、别全开（噪音大且烧日志盘）：

| 级别 | 能看到什么 |
|---|---|
| `oc_server=debug` | 每轮模型请求的 `shape`（角色序列 + 各条长度）、工具执行、截断续写、上下文压缩的落点 |
| `oc_llm=debug` | provider 建流、协议细节 |
| `oc_llm=trace` | provider 实际收发的原始报文（隐私敏感，排查「空回复/400」时才开） |

一条 run 内部由 `run{run_id=… session=…}` span 包裹，同一轮的中间行都带这个前缀，
跨行对齐就能拼出该轮的完整执行轨迹。

`oc debug` 关键字段：

- `writer OK/DOWN`：写线程是否存活
- `idem N`：幂等表条目数（不该单调增长）
- `phase`：会话当前阶段（`idle` / `streaming` / `tool-exec` / `awaiting-mdl` 等）
- `last_delta`：距最近一次进展多久（持续增大说明可能卡住）

---

## 故障手册

### 启动失败：单实例锁被占用

**现象**：`oc serve` 报「已有 oc serve 在运行」。

**处置**：确认是否真有另一个 daemon 进程在运行（`ps aux | grep "oc serve"`），停止它后重试。若进程已死但锁文件残留，删除 `~/.oc/run/oc.lock` 后重启。

---

### 无法连接 daemon

**现象**：运行 `oc`、`oc status` 等命令时报「无法连接 daemon：connection refused」。

**处置**：先运行 `oc serve`（可在另一个终端或 systemd）。确认 socket 路径一致——两端用的 `OC_SOCKET` 或 `oc serve --socket` 值应相同。

---

### `oc debug` 显示 `writer DOWN`

**现象**：`oc debug` 输出 `writer DOWN`，写操作（记忆保存、dreaming）返回错误。

**说明**：`writer DOWN` 说明 SQLite 写线程已崩溃，常见原因是磁盘满或数据库文件损坏。**读路径仍可用**，当前会话可继续对话但不保存记忆。

**处置**：

1. 查日志确认崩溃原因：`~/.oc/logs/`
2. 磁盘满则清理磁盘后重启：`oc serve`
3. 数据库文件损坏则备份旧库、重建：`oc doctor`（会创建新空库）

---

### 数据库结构过旧（`check_shape` 失败）

**现象**：`oc doctor` 报 `[err] 数据库结构与当前版本不符，缺少：<table_name>`，daemon 无法启动。

**说明**：开发阶段直接改了建表 DDL 而没有写 schema 迁移步骤，旧库版本号虽匹配但表结构已过时。

**处置**：停止 daemon，删库重建。

```bash
# 连 -wal / -shm 一起删
rm ~/.oc/oc.sqlite ~/.oc/oc.sqlite-wal ~/.oc/oc.sqlite-shm
oc doctor   # 按新 DDL 重建
```

---

### 回复中途截断 / 工具调用无响应

**现象**：模型开始回复但在某处停止，或宣布要调用工具后什么都没发生。

**诊断步骤**：

1. 运行 `oc debug`，看 `phase` 和 `last_delta`
2. 若 `phase = tool-exec` 且 `last_delta` 持续增大，说明工具在等待审批或执行超时
3. 若 `phase = awaiting-mdl` 且 `last_delta` 很大，说明模型响应慢或 API 超时

**常见原因与处置**：

- 工具等待审批：在 TUI 里看审批提示，按 y/n 回复；无人值守场景设 `[tools.approval] mode = "allow"`
- `max_output_tokens` 太小：查日志有无 `WARN max_tokens 截断`，调大 `config.toml` 里的 `max_output_tokens`
- API key 失效或额度耗尽：查 `~/.oc/logs/` 里有无 401/402 响应

---

### 卡死诊断 abort（看门狗触发）

**现象**：正在进行的 run 突然以错误结束，日志里有 `anti-stuck abort`。

**说明**：看门狗检测到 `last_delta` 超过 `abort_min_secs`（默认 300s）且满足其他条件时，主动 abort 该 run。这是正确的兜底行为。判据是「多久没有进展」，不是 run 总时长——生成大型文件期间持续输出不会被判为卡死。

**处置**：

- 若误杀了正常运行的慢 run：调大 `[watchdog] abort_min_secs`
- 若模型真的卡住了：查日志找根因（API 超时、工具卡住等）

---

### 提醒没响 / cron 任务不触发

**诊断**：

```bash
oc cron list   # 确认任务存在，查看下次触发时间（含时区）
```

**常见原因**：

- 时区配置错误：检查任务的时区列（`oc cron list` 输出含本地时区渲染后的时间）
- daemon 未运行：cron 靠 daemon 心跳驱动，daemon 停了没有 cron 触发
- 无人值守场景的审批超时：确认 `[tools.approval] timeout_secs` 不为 0

---

### Windows 命名管道冲突

**现象**：Windows 上多个 daemon 实例（开发时常见）互相干扰。

**原因**：默认管道名 `\\.\pipe\oc-daemon` 是全局常量，不随 `OC_HOME` 变化。

**处置**：用 `OC_SOCKET` 区分不同实例。

```bash
OC_SOCKET=\\.\pipe\oc-dev oc serve
OC_SOCKET=\\.\pipe\oc-dev oc
```
