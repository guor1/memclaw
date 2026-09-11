# 部署与配置

## 运行时目录布局

oc 的全部运行时产物存放在 `~/.oc/`（可用 `OC_HOME` 覆盖）：

```
~/.oc/
├── config.toml               主配置文件
├── oc.sqlite                 SQLite 数据库（含 -wal / -shm）
├── run/
│   ├── oc.lock               单实例文件锁（serve 持有）
│   └── oc.sock               AF_UNIX socket（Linux/macOS）
├── logs/
│   └── oc.log.YYYY-MM-DD     daemon 日志，按天滚动
├── soul/
│   ├── SOUL.md               agent 人格（onboard 生成）
│   ├── USER.md               用户偏好（agent 随对话更新）
│   ├── AGENTS.md             长期工作约定（手工编辑）
│   └── MEMORY.md             curated 核心记忆（dreaming 巩固重写）
├── workspace/                agent 工作目录（file/sys 工具允许根之一）
└── skills/
    └── *.md                  技能文件（文件名即技能名）
```

Windows 命名管道名：`\\.\pipe\oc-daemon`（全局常量，用 `OC_SOCKET` 覆盖）。

## 初始化

```bash
oc onboard   # 交互式生成 ~/.oc/ 骨架
oc doctor    # 校验配置 + 建库
```

## 启动与停止

```bash
# 启动 daemon（前台阻塞）
oc serve

# 连上 daemon 进 TUI
oc

# 启动 HTTP 兼容层（另一个终端，可选）
oc http --port 8080
```

停止 daemon：Ctrl-C 或 `kill <pid>`，正常关停会完成当前 run 后退出。

## 配置 API Key

推荐用环境变量，在 `config.toml` 里用 `{env="..."}` 引用：

```toml
[[models]]
alias = "default"
provider = "anthropic"
model = "claude-opus-5"
api_key = { env = "ANTHROPIC_API_KEY" }
```

或直接内联（不推荐提交到版本控制）：

```toml
api_key = { inline = "sk-ant-..." }
```

## 环境变量

| 变量 | 说明 |
|------|------|
| `OC_HOME` | 覆盖 `~/.oc/` 根目录 |
| `OC_SOCKET` | 覆盖 socket/管道路径，对 daemon 和所有客户端生效 |
| `RUST_LOG` | 日志级别，serve 默认 `oc=info,oc_server=info,oc_llm=info` |
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `DEEPSEEK_API_KEY` | DeepSeek API key |

## systemd（Linux 可选）

```ini
[Unit]
Description=oc daemon

[Service]
Type=simple
ExecStart=/usr/local/bin/oc serve
Restart=on-failure
Environment=ANTHROPIC_API_KEY=sk-ant-...

[Install]
WantedBy=default.target
```

## 配置修改

所有配置变更需重启 `oc serve` 生效，不支持热更。详细配置项说明见 [`docs/reference/config.md`](../reference/config.md)。
