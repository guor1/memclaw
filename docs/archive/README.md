# 归档文档

这里存放的是 2026-09-11 文档体系重构前的历史文档。它们**已冻结，不再更新**。

代码和决策的历史仍可通过 `git log -- docs/archive/` 追溯。

## 如何使用这些文档

如果你要了解某个功能或决策的来龙去脉，在这里翻比翻 git log 更直观。
如果你要查阅系统的当前状态，去 `docs/` 目录下的新文档，不要在这里找。

## 各文件说明

### plan/下一阶段计划.md

P0 / P1 / P2 三阶段的完整执行记录。末尾的「变更记录」节是 2026-08-29 到 2026-09-11 的 21 条工程日志，也是整个项目从可运行原型到生产收口阶段的最详细叙述。现行等价物：`CHANGELOG.md`（已发生的版本变更）+ `ROADMAP.md`（未完成的规划）。

### plan/P2-生产化收口详细计划.md

P2-1 到 P2-9 各项的背景/改动方案/验收标准/成本估算。P2-1 到 P2-5、P2-7 均已完成，代码可在对应 crate 找到。

### plan/P1-2-standing-intent方案.md

Standing intent 触发链的落地方案，含 zeroclaw 调研结论。

### plan/P1-7-代码质量与技术债.md

Clippy 清零 + TODO 归档，已完成。

### plan/P2-4-工具调用历史重放修复.md

工具调用历史重放缺陷的根因分析与修复方案（2026-09-08）。注意：该文件名里的 P2-4 与 P2 计划里 P2-4（FTS5 索引）是两件不同的事，历史遗留的编号冲突。

### plan/file-edit-append方案.md

file 工具 edit/append 操作的接口设计文档，对应 `feat(tools): file 加 edit/append` 提交。

### design/01-功能点清单.md

参照 OpenClaw 写的功能点盘点，是项目最初的需求来源文档。

### design/02-核心机制.md

OpenClaw 核心机制的逆向分析，每节带「→ Rust 建议」。现行等价物：`docs/architecture/`。

### design/03-Rust落地方案.md

crate 划分与技术选型的早期决策文档（v2 版）。

### design/04-详细设计文档.md

1088 行的逐模块详设，是最权威的原始设计文献。§1 架构总览、§11 记忆系统时序、§12 主动性时序质量最高，已提炼进 `docs/architecture/`。

### design/OpenAI-Responses-API-方案.md

oc-http 兼容层的协议映射设计文档，已提炼进 `docs/reference/protocol.md`。

### research/zeroclaw流式传输调研.md

P0-1 事件丢失修复前对参照项目流式传输机制的调研，直接影响了 RunSink 背压方案的选择。

### research/P1-2-改动范围审查.md

P1-2 standing intent 落地时的改动范围逐层拆解与风险评估。

### CODE_REVIEW_2026-09-02.md

2026-09-02 的全面代码评审报告（当时 207 个测试，评审结论直接催生了 P1-7 和 P2 计划）。注意：文件内的 7 处相对链接路径有误（多了一层 `docs/` 前缀），但不影响阅读。
