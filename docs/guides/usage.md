# 使用指南

覆盖日常会用到的全部功能。命令的完整参数见 [CLI 参考](../reference/cli.md)。

---

## 对话

启动 daemon 后另开终端运行 `oc` 进 TUI，或跑 `oc http` 后用浏览器打开 Web UI。回复是流式的，长回复会边生成边显示。

### 斜杠指令

斜杠指令（`/...`）在 **TUI 和 Web UI 都能用**。解析不在客户端——daemon 是唯一解析器，两个端只判断「是否 `/` 开头」就整条转发，所以两边指令集、文案、行为完全一致，新增指令改 daemon 一处即可。

| 指令 | 作用 |
|---|---|
| `/help` | 显示可用指令说明 |
| `/new` | 开一个新会话（旧会话保留，可用 `/session <id>` 切回） |
| `/session <id>` | 切换到指定会话（不存在则首次发送时创建） |
| `/sessions` | 列出所有会话 |
| `/clear` | 清空当前会话上下文（历史保留在库中，不再进提示词；别名 `/reset`） |
| `/compact` | 压缩当前上下文（摘要旧历史，保留语义） |
| `/stop` | 中止当前进行中的回合 |
| `/status` | 运行状态：活跃 run / 排队 / 上下文用量 / 模型 |
| `/model` | 当前生效的模型与 provider / 端点（只读） |
| `/tasks` | 列出后台任务 |
| `/cron list` | 列出定时任务 |
| `/intent list` | 列出话题待办 |
| `/memory search <q>` | 按关键词检索记忆 |
| `/whoami` | 显示当前会话 id（别名 `/id`） |

未识别的斜杠指令**不会**被当成聊天内容发出去，daemon 会返回提示并引导看 `/help`。

`/new` 生成的会话 id 是本地时间戳（如 `s1757641234`），`/sessions` 里一眼能看出先后。

会话历史持久化在 SQLite 里，daemon 重启不失忆，重连即恢复上下文。

### 上下文压缩

长对话会自动压缩：旧历史被摘要模型压成一段摘要，腾出窗口继续聊。压缩前会把值得记住的内容沉淀为 episodic 记忆候选，不会随摘要一起丢掉。

手动触发：

```bash
oc compact          # 命令行，或 TUI/Web UI 里直接输 /compact
```

---

## 记忆

三层结构：

| 层 | 来源 | 注入方式 |
|---|---|---|
| curated | 用户显式「记住…」，或 dreaming 从 episodic 巩固上来 | 每轮都注入 |
| episodic | 系统从对话推断值得记住的情节 | 按相关性检索注入 |
| 短期 | 当前会话历史 | 直接在上下文里 |

### 显式记忆

对话里说「记住我喜欢简洁的回复」，这条直接进 curated。

**同主题会覆盖而非并列**：之后说「记住我改用 Neovim 了」，会顶掉之前的「我用 VS Code」，不会留下两条矛盾记录。判定靠词法抽取主题键，保守策略——抽不出主题时退回追加，避免误删。

### 检索记忆

```bash
oc memory search 简洁           # 词法检索
oc memory search 编辑器 --limit 5
```

### 文件形态的记忆

`~/.oc/soul/` 下四个文件参与上下文组装：

| 文件 | 谁维护 | 内容 |
|---|---|---|
| `SOUL.md` | onboard 生成，可手改 | agent 人格 |
| `USER.md` | agent 随对话更新 | 用户偏好 |
| `AGENTS.md` | 手工编辑 | 长期工作约定 |
| `MEMORY.md` | dreaming 巩固时重写 | curated 核心记忆 |

`MEMORY.md` 被 dreaming 重写时用乐观并发：落盘前重新校验文件哈希，如果你在此期间手改过，改为追加而不覆盖你的编辑。

机制细节见 [记忆架构](../architecture/memory.md)。

---

## 技能

技能是「教 oc 做某类任务的固定流程」，一个技能 = 一个目录 + 一份 `SKILL.md`（YAML frontmatter + Markdown 正文）。

### 安装

三种方式等价，最终都是把目录放进 `~/.oc/skills/`，重启 `oc serve` 后生效：

```bash
# 1) 从 ClawHub 安装（scoped 包，落在 @<publisher>/<slug>/）
clawhub install @pskoett/self-improving-agent

# 2) 从 Git 仓库安装单个技能
npx skills add https://github.com/anthropics/skills --skill frontend-design

# 3) 手工放入（裸包）
mkdir -p ~/.oc/skills/my-skill
cat > ~/.oc/skills/my-skill/SKILL.md <<'EOF'
---
name: my-skill
description: 这个技能做什么、什么时候该用。
enabled: true
---

# 正文：具体流程，模型按需读取
EOF
```

安装后**必须重启 `oc serve`** 才重新扫描（技能是启动时加载的）。

### 目录结构

技能在磁盘上有两种形态，取决于 slug 是否带 scope（对齐 ClawHub 的 npm 风格 slug）：

| 形态 | 磁盘路径 | slug |
|---|---|---|
| 裸包 | `~/.oc/skills/<name>/SKILL.md` | `<name>` |
| scoped 包 | `~/.oc/skills/@<publisher>/<name>/SKILL.md` | `@<publisher>/<name>` |

`SKILL.md` 的 frontmatter 支持：`name`（展示名，缺省回退叶子目录名）、`description`（进可用技能列表）、`enabled`（默认 true）、`metadata.openclaw.os`（平台限制，如 `["darwin"]`）。

### 怎么生效

技能正文**不会**整段塞进上下文。系统提示词里只放一份索引（技能名 + 描述 + 内容指纹），模型判断该用某个技能时，用 `file` 工具读 `~/.oc/skills/<slug>/SKILL.md` 拿全文——正文变了指纹会变，模型据此重读。

### 开关与门控

`config.toml` 的 `[skills]` 节控制哪些技能可见：

```toml
[skills]
allowlist = []     # 非空则只加载列表内的技能
denylist  = []     # 永不加载（优先于 allowlist）
```

列表项填**完整 slug**（scoped 包要带 `@scope/` 前缀）：

```toml
[skills]
denylist = ["@pskoett/self-improving-agent"]   # 不是 "self-improving-agent"
```

`metadata.openclaw.os` 与当前系统不匹配的技能会被跳过。

---

## 定时任务

到点触发，daemon 主动推送，不占用当前对话。

```bash
oc cron add "0 9 * * 1-5" "提醒我看今天的日程"      # 工作日 9 点
oc cron add "30 21 * * *" "该睡了" --tz Asia/Shanghai
oc cron list                                        # 含本地时间与剩余时长
oc cron rm <id>
```

表达式是 5 字段 cron（分 时 日 月 周），时/分按指定时区的**本地时间**解释。省略 `--tz` 用本机时区。

模型也能直接创建定时任务——对话里说「12:50 提醒我喝水」或「90 秒后叫我」，它会登记 cron 而不是在对话里干等。亚分钟级的一次性延时也支持。

---

## 话题触发提醒

和 cron 的区别：cron 到点触发，intent 由**聊到相关话题**触发。

```bash
oc intent add "带转换插头" 出差 德国
oc intent list
oc intent rm <id>
```

之后聊到「出差」或「德国」时，提醒会作为隐藏上下文注入，让模型自然带出来。

防打扰三重控制：

| 参数 | 作用 |
|---|---|
| `--cooldown-secs` | 两次提醒的最小间隔 |
| `--max-fires` | 最多提醒几次，用尽即静默 |
| `--expire-days` | 多少天后过期，`0` = 永不过期 |

省略则取 `config.toml` 的 `[proactive]` 默认值。

---

## 工具与审批

模型可以读写文件、执行命令、抓网页。危险操作会弹审批，在 TUI 里按 `y` / `n` 回复。

审批策略在 `config.toml` 的 `[tools.approval]` 配置。无人值守场景（比如通过 HTTP 网关调用）需要把 `mode` 设为 `allow`，否则审批等不到人回复会超时。

文件工具的工作区固定在 `~/.oc/workspace`，不会继承 daemon 的启动目录。

---

## HTTP 网关

让标准 OpenAI 客户端直接对话：

```bash
oc http --port 8080
```

```bash
curl http://127.0.0.1:8080/v1/responses \
  -H 'content-type: application/json' \
  -d '{"input": "今天有什么待办？"}'
```

自动化脚本建议显式指定会话，别挤 `main`：

```bash
curl http://127.0.0.1:8080/v1/responses \
  -H 'x-openclaw-session-key: my-script' \
  -H 'content-type: application/json' \
  -d '{"input": "整理这份日志", "stream": true}'
```

网关绑定 `127.0.0.1` 且**无鉴权**——能访问本机端口的进程都有完整权限。完整接口契约见 [协议参考](../reference/protocol.md)。

---

## 状态与诊断

```bash
oc status            # 会话状态：活跃 run、排队数、上下文用量
oc sessions          # 列出所有会话
oc debug             # 诊断快照：writer 健康、每会话 phase
oc debug --watch     # 每秒刷新，看卡住时状态怎么演变
```

「不回复 / 卡住了」先看 `oc debug --watch`，判读方法见 [故障排查](../operations/troubleshooting.md)。

日志在 `~/.oc/logs/`，调高级别：

```bash
RUST_LOG=oc_server=debug oc serve
```
