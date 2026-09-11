# memclaw 项目代码评审报告

> 评审日期：2026-09-02  
> 评审范围：全仓代码 + 设计文档 + 最近 30 次提交  
> 测试状态：207 个测试全绿  
> 开发阶段：M1-M6 + P0 + P1-1~P1-5 已落地

---

## 📊 执行摘要

**综合评分**：8.1/10

**项目定位**：架构优秀、工程严谨、迭代健康的早期项目，正处于从"功能完整"到"生产可用"的关键转折点。

**核心优势**：
- ✅ 纯核心设计：领域逻辑 100% 确定性可测，单向依赖编译期强制
- ✅ 文档与代码同步：设计文档 1050 行，每个 P1 功能都有测试用例文档
- ✅ 问题修复彻底：P1-5 首版上线当晚发现 3 个独立缺陷并全部修复

**主要风险**：
- 🔴 P1-6（episodic 产出）是 dreaming 的上游断点，需立即解决
- 🔴 读写分离、写线程自愈是可用性瓶颈，建议提前到 P2 前完成
- 🟡 4 条 clippy 警告虽小但积累会侵蚀代码质量，应保持零警告

---

## 🎯 量化指标

| 维度 | 评分 | 说明 |
|------|------|------|
| 架构设计 | 9.0/10 | 单向依赖严格、纯核心设计优秀 |
| 代码质量 | 7.5/10 | 逻辑清晰但有 clippy 警告和 TODO |
| 测试覆盖 | 8.5/10 | 207 测试全绿，关键路径有回归 |
| 文档完善度 | 9.0/10 | 设计文档 + 测试用例 + 变更记录齐全 |
| 可维护性 | 8.0/10 | 模块化好但部分技术债需还 |
| 性能 | 7.0/10 | 读写未分离，慢查询会阻塞 |
| 安全性 | 8.0/10 | 审批门、provenance 分级、审计链到位 |

---

## ✅ 优秀实践

### 1. 架构设计（9/10）

**单向依赖严格执行**
```
oc-cli → oc-tui → oc-server → oc-core (纯策略)
                      ↓         ↓
                  oc-store  oc-tools
                      ↓
                  oc-llm
```

- oc-core 完全纯净：无 IO、无时钟、无随机
- workspace 依赖图在编译期强制无环
- 所有外部量（时间、随机、模型输出）作为参数注入

**关注点分离清晰**
- `oc-proto`：纯 DTO + JSON Schema，无逻辑
- `oc-core`：纯策略函数，输出决策而非执行
- `oc-server`：唯一的编排层，负责副作用执行
- `oc-store`：单库 + 单写线程

### 2. 工程质量（8.5/10）

**测试覆盖充分**
- 207 个测试（core 纯函数单测 + server 端到端集成测试）
- 关键路径有回归测试：长回复不截断、审批取消、并发提交、cron 触发
- 测试命名清晰：`abort_interrupts_pending_approval_and_cleans_registry`

**文档完善**
- README 结构清晰：特性 → 架构 → 使用 → 开发
- 设计文档详尽（04-详细设计文档.md，1050 行）
- 每个 P1 功能都有配套测试用例文档
- 变更记录追踪完整

**可观测性到位**
- 结构化日志落盘（`~/.oc/logs/oc.log.YYYY-MM-DD`）
- `oc debug --watch`：实时诊断快照
- `oc doctor`：环境校验、schema 导出

### 3. 迭代节奏（9/10）

**渐进式交付**
- M1-M6 里程碑清晰，每个 M 都有可演示验收项
- P0 稳定核心闭环 → P1 补齐助手功能 → P2 生产化收口
- 优先级明确：先修核心路径 bug，再堆新功能

**问题修复彻底**
- P1-5 首版上线后立即真机验证，当晚发现三个独立缺陷并全部修复
- P0-1 事件丢失问题：调研 zeroclaw 传输设计后采用「per-run 背压流」根本解决

---

## 🔴 High 严重程度问题

### H-1: episodic 记忆产出缺失（功能断点）

**位置**: 全仓  
**影响**: P1-4 已完成的 dreaming 巩固能力在真机上永远空转

**问题描述**:
```rust
// 全仓没有任何代码写入 tier='episodic'
// persist_explicit_memory 直接写 curated，绕过了 episodic→巩固链
// 导致 dream_candidates 永远返回空集
```

**建议**:
1. 落地设计 §11.5：在 `session.reset` 或压缩前 flush episodic 候选
2. 判定逻辑用纯函数（哪些算"值得记住"），IO 在 store，调度在 session
3. **优先级**：计划 P1-6 已列，应立即排期（2-3 天）

**关联文档**: [P1-6](docs/plan/下一阶段计划.md#p1-6-episodic-记忆产出)

---

### H-2: Clippy 警告未清零

**位置**: 
- `crates/oc-core/src/config.rs:87` - if_same_then_else
- `crates/oc-llm/src/sse.rs:25` - while_let_loop  
- `crates/oc-store/src/types.rs:44,103,131` - should_implement_trait（3 处）

**影响**: 代码质量债务累积，降低新人信心

**建议**:
```rust
// config.rs:87 - 合并重复分支（5 分钟）
} else if m.contains("gpt-4o") || m.contains("gpt-4.1") || m.contains("o1") 
       || m.contains("o3") || m.contains("gpt-4-turbo") || m.contains("gpt-4-1106") {
    128_000

// sse.rs:25 - 改用 while let（5 分钟）
while let Some(pos) = self.buf.find('\n') {
    // ...
}

// types.rs - 实现 FromStr trait（30 分钟）
impl std::str::FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> { /* ... */ }
}
```

**验收**: `cargo clippy --workspace --all-targets` 零警告

**优先级**: 高，快速胜利（总计 1-2 小时）

**关联文档**: [P1-7 代码质量](docs/plan/P1-7-代码质量与技术债.md#1-clippy-警告清零)

---

### H-3: TODO 标记未清理

**位置**: `crates/oc-server/src/registry.rs:9`  
```rust
//! **不做空闲淘汰**：单用户短期无碍；长期可加 LRU/TTL（留 TODO）。
```

**建议**: 
- P2 计划已有"会话 actor 空闲淘汰"项
- 要么删除此 TODO 标记，要么改写为指向 P2 计划
- **成本**: 5 分钟

**关联文档**: [P1-7 代码质量](docs/plan/P1-7-代码质量与技术债.md#2-todo-标记归档)

---

## 🟠 Medium 严重程度问题

### M-1: 读写未分离（与设计文档矛盾）

**位置**: `oc-store`  
**设计承诺**: 读走短连接或小连接池（设计 §3.1）  
**实际**: 读写共用单写线程，慢查询阻塞全体会话

**影响**: 大型记忆检索会拖累主会话响应

**建议**:
1. 读操作包 `spawn_blocking` + 短连接/小连接池
2. 写保持单写线程不变（WAL 模式读不阻塞写）
3. **成本**: 3-5 天
4. **优先级**: P2-1，应提前到 P1 后立即做

**关联文档**: [P2-1 读写分离](docs/plan/P2-生产化收口详细计划.md#p2-1-读写分离)

---

### M-2: 写线程 panic 无自愈

**位置**: `oc-store` WriterActor  

**问题**: JoinHandle 被丢弃，panic 后无人监控 → 存储层瘫痪需重启

**建议**:
```rust
// 监控 JoinHandle
let handle = tokio::spawn(async move {
    if let Err(e) = writer_loop(rx).await {
        error!("Writer thread failed: {}", e);
    }
    health.store(false, Ordering::SeqCst);
});

// 每个写操作先健康检查
async fn upsert_memory(&self, m: NewMemory) -> StoreResult<MemoryId> {
    self.check_health().await?;
    // ...
}
```

**成本**: 1-2 天  
**优先级**: P2-2，影响 7×24 运行

**关联文档**: [P2-2 写线程自愈](docs/plan/P2-生产化收口详细计划.md#p2-2-写线程-panic-自愈)

---

### M-3: IdempotencyCache + SessionActor 无淘汰

**位置**: `oc-server`  

**问题**: 长期运行内存单调增长

**建议**:
- IdempotencyCache 加 1h TTL + 后台 GC 任务
- SessionActor 24h 无活动淘汰（主会话永不淘汰）
- **成本**: 2-3 天

**关联文档**: [P2-3 内存淘汰](docs/plan/P2-生产化收口详细计划.md#p2-3-idempotencycache--sessionactor-淘汰)

---

### M-4: 配置热更未接线

**位置**: `oc-server`  

**问题**: 代码中有 `ArcSwap<Config>` 类型定义，但无实际热更逻辑

**建议**:
- 方案 A：实现热更（2-3 天，配置文件 watch + reload）
- 方案 B：改用普通 `Arc` 并在文档说明"需重启"（1 小时）
- **推荐**: 方案 B（单用户场景重启成本低）

**关联文档**: [P2-5 配置热更](docs/plan/P2-生产化收口详细计划.md#p2-5-配置热更接线或移除-arcswap)

---

## 🟢 Low 严重程度 / 改进建议

### L-1: USER.md 仍是死文件
- onboard 创建但无人读写，偏好实际在 `memory` 表
- **建议**: P1-4 已完成 MEMORY.md 重写，USER.md 可用类似机制

### L-2: cron day-of-month/day-of-week 语义简化
- 标准 cron 两者非 `*` 时取 OR，当前实现取 AND
- **影响**: 低（个人助手场景多数只用其一）

### L-3: 错误处理一致性
- core 层多用裸 `String` 作错误
- **建议**: 统一用结构化错误枚举（2-3 天，非强制）

### L-4: 向量检索未启用
- `memory_vec` 表已建但未使用
- **依赖**: 需先定 embedding 模型
- **优先级**: P2-9，MVP 纯词法已可用

---

## 🚀 行动计划（按优先级）

### 立即行动（本周内）
1. ✅ **P1-7 Clippy 警告清零**（1-2 小时）
   - 合并重复分支、改用 while let、实现 FromStr trait
   - 快速胜利，代码质量基线

2. 🔴 **P1-6 episodic 产出**（2-3 天）
   - 解除 dreaming 上游断点
   - 让 P1-4 真正闭环

### 短期（2 周内）
3. **P1 真机复验**（1 天）
   - P1-2/P1-3/P1-4/P1-5 目前只有自动化覆盖
   - 历史上缺陷都是真机才暴露

4. 🔴 **P2-1 读写分离**（3-5 天）
   - 避免慢查询拖累主会话
   - 可用性基石

### 中期（1 个月内）
5. **P2-2 写线程自愈**（1-2 天）
6. **P2-3 IdempotencyCache + SessionActor 淘汰**（2-3 天）
7. **P2-4 memory.text 索引**（2-3 天）
8. **真机 7 天测试**：验证内存稳定、无 panic、响应时间正常

### 长期（P2 后续）
9. 双平台 CI matrix
10. 向量检索接入（需先定 embedding 方案）
11. Provider failover 接线

---

## 📋 验收清单

### P1 收尾
- [ ] `cargo clippy --workspace --all-targets` 零警告
- [ ] episodic 产出落地，dreaming 真机可用
- [ ] P1-2/P1-3/P1-4/P1-5 真机复验通过

### P2 阶段 1（可用性基石）
- [ ] 读写分离：慢查询不阻塞主会话
- [ ] 写线程自愈：panic 后降级到只读模式
- [ ] 内存淘汰：无泄漏
- [ ] 真机 7 天测试通过

### P2 阶段 2（工程化收口）
- [ ] memory.text FTS5 索引：10 万条 <100ms
- [ ] 双平台 CI 绿：Linux + Windows
- [ ] 文档齐全：部署指南 + 运维手册

---

## 💡 最终评价

**这是一个架构优秀、工程严谨、迭代健康的早期项目。**

**为什么架构优秀**：
- 纯核心设计让领域逻辑 100% 可测，这是长期可维护性的基石
- 单向依赖编译期强制，避免循环依赖的泥潭
- 关注点分离清晰，每个 crate 职责单一

**为什么工程严谨**：
- 207 个测试覆盖关键路径，测试命名清晰表意
- 文档与代码同步演进，变更记录追踪完整
- 可观测性到位：日志、诊断快照、环境校验

**为什么迭代健康**：
- P0 稳定核心闭环的优先级判断正确：先修根基，再堆功能
- 问题修复彻底：P1-5 真机验证当晚修复 3 个缺陷
- 调研驱动决策：zeroclaw 传输调研 → per-run 背压流方案

**下一步关键动作**：
1. P1-7（clippy 清零）→ P1-6（episodic 产出）→ P1 真机复验
2. P2 阶段 1（读写分离 + 自愈 + 淘汰）→ 真机 7 天测试
3. P2 阶段 2（索引 + CI）→ v0.1.0 发布

项目当前处于从"功能完整"到"生产可用"的关键转折点。把 P1-6 + P1-7 + P2 阶段 1 做完，即可进入真实长期运行验证阶段。

---

## 附录：评审方法

**评审范围**：
- 代码：8 个 crate（oc-proto, oc-core, oc-store, oc-llm, oc-tools, oc-server, oc-tui, oc-cli）
- 文档：README, 设计文档（1050 行）, 计划文档, 测试用例文档
- 提交历史：最近 30 次提交（2026-08-29 至 2026-09-01）

**评审维度**：
1. 架构设计：依赖关系、模块划分、纯核心设计的落地质量
2. 代码质量：命名、注释、错误处理、类型设计
3. 测试覆盖：单元测试、集成测试的完整性和质量
4. 可维护性：代码重复、复杂度、扩展性
5. 性能与并发：资源管理、并发安全、性能瓶颈
6. 安全性：输入验证、权限控制、数据安全
7. 文档质量：代码注释、README、设计文档

**工具使用**：
- `cargo clippy --workspace --all-targets`：静态分析
- `rg 'TODO|FIXME|XXX|HACK|BUG'`：技术债标记
- 手动审读：核心模块（agent.rs, session.rs, ops.rs）

---

**评审人**: Claude (Opus 5)  
**评审日期**: 2026-09-02  
**项目状态**: 207 测试全绿，M1-M6 + P0 + P1-1~P1-5 已落地
