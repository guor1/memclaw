# 任务看板

新需求、缺陷、规划都记在这里。**后续开发只看这一份**——任务条目在表格，方案与设计要点在「条目明细」。`docs/` 现行文档只描述已落地状态，`docs/archive/` 已冻结不再更新。

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

### 功能

| ID | 标题 | 优先级 | 现状 |
|---|---|---|---|
| FEAT-1 | Provider failover 接线 | P1 | `oc-core/src/model.rs` 的 `failover()` / `resolve()` 已实现且有单测，但全仓无调用方。接线点在 `oc-llm` 重试路径 |

### 重构

| ID | 标题 | 优先级 | 现状 |
|---|---|---|---|
| REFACTOR-1 | 抽出 `oc-core::context` 模块 | P3 | 上下文加载散落在 `oc-server/src/session.rs`，不阻塞功能，纯整洁性 |

---

## 已决定暂缓 / 不做

这些**不是待办**，是「为什么现在不做」的结论——记下来是为了别再重提。要动手做某条，先拆成 `FEAT-*` / `REFACTOR-*` 再进 TODO。来源 `docs/archive/design/01-功能点清单.md`（参照 OpenClaw 的功能盘点）。

| 主题 | 决定 |
|---|---|
| 多渠道（Telegram/Discord 等）§2 | 个人助手定位下单源够用，暂缓 |
| 沙箱（docker/ssh/openshell）§6.3 | 不引入沙箱，靠审批门（`ApprovalMode`）兜底 |
| 插件系统 §15 | `plugin-agnostic` 是设计前提，但当前无外部插件消费方，不做 |
| Hooks §14 | 无 hook 机制，扩展接缝未做 |
| MCP client/server §7 | 未做 |
| 多智能体路由 + 委派 §3 | 单 agent 模式 |
| ClawHub §8/§15 | 外部服务，不在单仓库内可完成；前置 skills 格式（已落地）+ 插件 manifest（未做） |
| 媒体/语音实时、节点配对、Canvas 等 §16~§20 | 未做，P3 愿望清单 |

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

---

## 维护约定

- 新条目加到 TODO 对应分类下，ID 用 `类型-序号`（`BUG` / `FEAT` / `TEST` / `OPS` / `REFACTOR`）
- 开始做就移到 DOING，卡住移到 BLOCKED 并写明卡在什么
- 做完从本文件删除，把变更写进 `CHANGELOG.md`
- 需要展开的写「条目明细」，简单的只留表格行
- 「已决定暂缓 / 不做」记的是结论，不是待办；要动手做某条，先拆成 `FEAT-*` / `REFACTOR-*` 再进 TODO
- **后续开发只看本文件**：新需求、方案、设计都写在这里——任务条目进对应表格，方案/设计要点写进「条目明细」。不在 `docs/` 下新建文档
- **`docs/archive/` 已冻结**，不再更新，只在追溯历史决策时翻；`docs/` 现行文档仅描述已落地的当前状态
- **本文件是后续开发的唯一入口**，`CHANGELOG.md` 只记已发布的历史
