# 快速开始

从零到能对话的最短路径。

## 前置

Rust 1.90+（从源码构建），或直接下载 release 二进制。详见 [安装](../operations/install.md)。

## 三步走

```bash
oc onboard    # 交互式初始化，生成 ~/.oc/ 骨架（config.toml + SOUL.md 等）
oc doctor     # 校验配置 + 建库，确认没问题
oc serve      # 启动 daemon（前台阻塞，Ctrl-C 停止）
```

`oc serve` 会占住这个终端，**另开一个终端**：

```bash
oc            # 连上 daemon，进 TUI 对话
```

## 配 API key

`oc onboard` 会问。之后也可以改 `~/.oc/config.toml`：

```toml
[[models]]
alias = "default"
provider = "openai"
model = "deepseek-chat"
base_url = "https://api.deepseek.com"
api_key = { env = "DEEPSEEK_API_KEY" }
```

改完要重启 `oc serve`，配置不支持热更。

## 验证能跑

在 TUI 里说句话，应该看到流式回复。

再验证记忆：说「记住我喜欢简洁的回复」，然后另一个终端：

```bash
oc memory search 简洁
```

应该能查到刚写进去的这条。

## 下一步

- [使用指南](usage.md) —— 记忆、定时提醒、工具审批、HTTP 网关等完整功能
- [CLI 参考](../reference/cli.md) —— 所有命令与参数
- [故障排查](../operations/troubleshooting.md) —— 出问题时先看这里
