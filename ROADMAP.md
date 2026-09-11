# 路线图

未完成的工作，按主题分类。不写日期承诺，做完从这里删掉，添加到 `CHANGELOG.md`。

---

## 稳定性验收

**真机 7 天连续运行观察**

当前 P2 阶段 1（读写分离 / 写线程自愈 / 内存淘汰）的代码已落地，但没有跑满 7 天验收。需要挂着跑并观察：

- `oc debug` 的 `idem` 计数和会话行数是否单调上涨（判断内存泄漏）
- 日志里有无 panic 或写线程降级告警
- 慢查询（大型记忆检索）是否阻塞主会话回复

验收工具：`oc debug --watch`（每秒刷新）和 `~/.oc/logs/`。

**dreaming 闭环真机验证（P1-6）**

episodic 产出需要 use_count 累积加上至少 3 天沉淀才能观察到 dreaming 巩固效果。自动化测试只证明了候选落库，端到端晋升链需要真机跑才看得到。

**两条未覆盖的自动化用例**

- `TC-P1-3f`：老库 v1 升级不丢记忆（需要真实 v1 数据库文件）
- `TC-H12`：真实 OpenAI SDK 打通（需要真实 API key 的 e2e 测试）

---

## 功能增强

**Provider failover 接线**

`crates/oc-core/src/model.rs` 里的 `failover()` 和 `resolve()` 纯函数已实现（含单元测试），但全仓没有调用方接线——模型请求失败时不会自动切换备用 provider。接线点在 `oc-llm` 的重试路径。

**向量语义检索（P2-9）**

`sqlite-vec` feature 的 schema 和条件编译代码已占位，但 feature flag 未启用。等确定 embedding 来源（provider API 还是本地小模型）后开启。在此之前，FTS5 词法检索已够用。

---

## 重构

**`oc-core::context` 模块抽取（P2-8）**

上下文加载逻辑散落在 `crates/oc-server/src/session.rs`，设计文档里计划抽到 `oc-core::context`，但还没做。不阻塞任何功能，纯内部整洁性工作。

**cron day-of-month / day-of-week 语义修正**

当前 `next_fire` 对同时指定两个字段时用 AND 语义，cron 标准（Vixie cron / cronlib）应为 OR。会影响「每月 1 号或每周一」这类表达式的解释，但日常用法影响有限。

---

## Windows smoke CI 腿

`scripts/e2e/smoke.sh` 已支持 Windows，但 `ci.yml` 的 smoke job 只在 ubuntu 跑，Windows 的进程级冒烟没有 CI 覆盖。
