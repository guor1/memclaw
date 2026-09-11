# 文档索引

## guides/ — 使用指南

| 文档 | 内容 |
|---|---|
| [quickstart.md](guides/quickstart.md) | 从零到能对话的最短路径 |
| [usage.md](guides/usage.md) | 完整功能：记忆、定时任务、工具审批、HTTP 网关、状态诊断 |

## reference/ — 参考手册

| 文档 | 内容 |
|---|---|
| [cli.md](reference/cli.md) | 所有子命令、参数、环境变量 |
| [config.md](reference/config.md) | config.toml 全字段说明 |
| [protocol.md](reference/protocol.md) | HTTP 网关接口（OpenAI Responses API 兼容） |

## architecture/ — 架构

| 文档 | 内容 |
|---|---|
| [overview.md](architecture/overview.md) | crate 划分、核心不变量、进程模型、车道模型 |
| [memory.md](architecture/memory.md) | 三层记忆、Lane1 检索、dreaming 巩固、FTS5 索引 |
| [agent-loop.md](architecture/agent-loop.md) | Agent 循环、工具集、审批门、防卡死机制 |
| [proactive.md](architecture/proactive.md) | cron、standing intent、心跳底座、anti-nagging |
| [store.md](architecture/store.md) | 单写线程、读写分离、写线程健康、FTS5 实现细节 |
| [adr/0001-per-run-sink-backpressure.md](architecture/adr/0001-per-run-sink-backpressure.md) | per-run 背压通道（P0-1 事件丢失修复）|
| [adr/0002-fts5-trigram-tokenization.md](architecture/adr/0002-fts5-trigram-tokenization.md) | FTS5 2-gram 预切词策略（P2-4）|
| [adr/0003-no-config-hotreload.md](architecture/adr/0003-no-config-hotreload.md) | 不做配置热更（P2-5）|

## operations/ — 运维

| 文档 | 内容 |
|---|---|
| [install.md](operations/install.md) | 安装：release 二进制 / 从源码构建 / 各平台差异 |
| [deploy.md](operations/deploy.md) | 部署：目录布局、初始化、启动停止、API key、systemd |
| [troubleshooting.md](operations/troubleshooting.md) | 故障手册：逐条现象 → 原因 → 处置步骤 |

## development/ — 开发

| 文档 | 内容 |
|---|---|
| [testing.md](development/testing.md) | 测试策略、如何跑、覆盖矩阵、环境陷阱 |
| [manual-probes.md](development/manual-probes.md) | 人工探针：需要眼睛判断的 4 条验证 |
| [release.md](development/release.md) | 发布流程：tag、CI、多平台产物 |

## archive/ — 归档

旧文档，已冻结，不再更新。详见 [archive/README.md](archive/README.md)。

---

任务看板（新需求 / 缺陷 / 规划）：[../BOARD.md](../BOARD.md)
