# 任务看板

新需求、缺陷、规划都记在这里。**看这一份就够了解后续开发。**

状态：`TODO` 未开始 · `DOING` 进行中 · `BLOCKED` 卡住（写明卡在什么） · `REVIEW` 待验收 · `DONE` 完成后移到 CHANGELOG 并从本文件删除

优先级：`P0` 阻塞发布 · `P1` 该做 · `P2` 想做 · `P3` 有空再说

---

## DOING

（空）

---

## BLOCKED

| ID | 标题 | 优先级 | 卡在什么 |
|---|---|---|---|
| OPS-1 | 真机 7 天连续运行验收 | P0 | 需要一台机器挂着跑满 7 天，只能等时间 |
| OPS-2 | dreaming 闭环真机验证 | P0 | 需要 use_count 累积 + ≥3 天沉淀，只能等时间 |
| FEAT-2 | 向量语义检索接入 | P3 | 需先定 embedding 来源（provider API 还是本地小模型），决策未做 |

---

## TODO

### 缺陷

| ID | 标题 | 优先级 | 影响 |
|---|---|---|---|
| BUG-1 | cron day-of-month / day-of-week 取 AND，标准应为 OR | P2 | 「每月 1 号或每周一」这类表达式解释错误。日常用法多数只用其一，影响有限 |

### 测试覆盖

| ID | 标题 | 优先级 | 缺什么 |
|---|---|---|---|
| TEST-1 | `TC-P1-3f` 老库 v1 升级不丢记忆 | P1 | 需签入一个真实 v1 schema 的 fixture 库 |
| TEST-2 | `TC-H12` 真实 OpenAI SDK 打通 | P1 | 需 live lane + 真 API key |
| TEST-3 | Windows 进程级冒烟进 CI | P2 | `smoke.sh` 已支持 Windows，`ci.yml` 只在 ubuntu 挂了 smoke job |

### 功能

| ID | 标题 | 优先级 | 现状 |
|---|---|---|---|
| FEAT-1 | Provider failover 接线 | P1 | `oc-core/src/model.rs` 的 `failover()` / `resolve()` 已实现且有单测，但全仓无调用方。接线点在 `oc-llm` 重试路径 |

### 重构

| ID | 标题 | 优先级 | 现状 |
|---|---|---|---|
| REFACTOR-1 | 抽出 `oc-core::context` 模块 | P3 | 上下文加载散落在 `oc-server/src/session.rs`，不阻塞功能，纯整洁性 |

---

## 条目明细

需要展开说明的条目写在这里，简单条目只留表格行即可。

### OPS-1 真机 7 天连续运行验收

P2 阶段 1（读写分离 / 写线程自愈 / 内存淘汰）代码已落地，但没跑满 7 天。这是 P2 出口标准里唯一还没达成的硬性条件。

挂着跑，观察三件事：

- `oc debug` 的 `idem` 计数和会话行数**不该单调上涨**（判断内存泄漏）
- 日志里**不该有** panic 或写线程降级告警
- 慢查询（大型记忆检索）**不该**阻塞主会话回复

工具：`oc debug --watch` 每秒刷新，日志在 `~/.oc/logs/`。

### OPS-2 dreaming 闭环真机验证

自动化测试只证明了 episodic 候选**落库且形状对**。能否通过门 1 取决于 `use_count` 累积和 ≥3 天沉淀，只有真机挂着跑才看得到完整巩固闭环。

历史上 P0-1 / P1-1 / P1-5 的缺陷全都是真机才暴露的，这条不能靠自动化替代。

### FEAT-1 Provider failover 接线

纯函数已就绪：

- `ModelCatalog::failover(tried) -> Option<&ModelEntry>`
- `ModelCatalog::resolve(alias)`

两者都有单测（`failover_skips_tried`），但 `oc-server` / `oc-llm` / `oc-cli` 里对 `.failover(` 零调用。

接线后的效果：主模型返回 5xx 时自动切到备用 provider，failover 链耗尽才报错。配置形态需要在 `[[models]]` 里加 `failover = ["openai:gpt-4o"]` 之类的字段。

### BUG-1 cron 字段语义

标准 cron（Vixie cron）在 day-of-month 和 day-of-week **都非 `*`** 时取 OR，当前 `next_fire` 实现取 AND。

修的时候注意别碰坏已有的东八区回归和 DST 跳表用例。

---

## 维护约定

- 新条目加到 TODO 对应分类下，ID 用 `类型-序号`（`BUG` / `FEAT` / `TEST` / `OPS` / `REFACTOR`）
- 开始做就移到 DOING，卡住移到 BLOCKED 并写明卡在什么
- 做完从本文件删除，把变更写进 `CHANGELOG.md`
- 需要展开的写「条目明细」，简单的只留表格行
- **本文件是后续开发的唯一入口**，`CHANGELOG.md` 只记已发布的历史
