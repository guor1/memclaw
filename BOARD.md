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

## 规划（路线里程碑）

`docs/archive/design/01-功能点清单.md`（参照 OpenClaw 的功能盘点）里标【必须】/【加分】但尚未落地的主题，按主题归并成里程碑。「来源」指向该清单对应章节。做完一项从本表删除、写进 `CHANGELOG.md`；要动手做时把里程碑拆成具体的 `FEAT-*` / `REFACTOR-*` 条目。

| ID | 主题 | 优先级 | 来源 | 现状 |
|---|---|---|---|---|
| ROAD-1 | Skills 完整形态（SKILL.md 格式 + `<available_skills>` 按需注入 + 资格门控） | P1 | §8 | 已完成：目录式 SKILL.md + frontmatter + `<available_skills>` 按需注入 + enabled/config/os 门控；env/bins 门控留后续 |
| ROAD-2 | 渠道抽象 + 首个真实渠道（Telegram/Discord 等） | P2 | §2 | 无渠道层。当前入口仅 CLI/TUI + loopback HTTP（`oc http` 原生 API + 内嵌 Web UI）。个人助手定位下单源够用，多源暂缓 |
| ROAD-3 | 插件系统（manifest 发现 + bundle plugin + 注册能力） | P2 | §15 | 无插件机制。`plugin-agnostic` 是设计前提，但当前无外部插件消费方 |
| ROAD-4 | Hooks（内部命令钩子 + 插件接缝） | P2 | §14 | 无 hook 机制。扩展接缝（`before_prompt_build` / `after_tool_call` 等）未做 |
| ROAD-5 | MCP client / server | P3 | §7 | 未做 |
| ROAD-6 | 沙箱（docker/ssh/openshell）+ elevated exec | P3 | §6.3 | 未做。当前 exec 无沙箱，靠审批门（`ApprovalMode`）兜底 |
| ROAD-7 | 多智能体路由 + 子智能体/委派 | P3 | §3 | 未做。单 agent 模式 |
| ROAD-8 | ClawHub（技能/插件发现安装 + 溯源审查） | P3 | §8/§15 | 未做。依赖 ROAD-1 + ROAD-3，且是外部服务 |
| ROAD-9 | 节点 & 配对（设备能力） | P3 | §16 | 未做 |
| ROAD-10 | 媒体 / 语音实时（TTS / 图音视频 / voice） | P3 | §17/§18 | 未做 |
| ROAD-11 | UI 增强（Canvas / A2UI / Control UI / 伴生 App） | P3 | §19 | 已有内嵌 Web UI（`oc-http/ui`，等价 WebChat），Canvas 等未做 |
| ROAD-12 | 可观测 / 安全增强（OTel/Prometheus 导出 + trajectory + `security audit` 命令） | P3 | §20 | 已有结构化日志 + audit 台账 + `oc debug`；导出与安全审计命令未做 |

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

### ROAD-1 Skills 完整形态

已完成【必须】三件，它们是 ClawHub（ROAD-8）能落地的前置：

1. **SKILL.md 标准格式**：`SkillBrief` 加 frontmatter（name/description/when_to_use/triggers 等），`skills_loader.rs` 解析元数据而非全文照读。
2. **`<available_skills>` 按需注入**：base prompt 里只放技能名 + `sha256` 版本标记，模型用 `file` 工具按需读正文（正文变了版本号变、触发重读）。这样 base prompt 保持精简。
3. **资格门控**：metadata/env/config/allowlist 决定哪些技能对本会话可见（enabled/config/os 已落地）。

留后续：env/bins 门控未做。依赖链：ROAD-1 → ROAD-8（ClawHub 装回来的就是带 frontmatter 的 SKILL.md 包，格式不统一则装回来也没法正确加载）。

### ROAD-8 ClawHub

外部服务，不在单仓库内可完成的范围。前置 ROAD-1（SKILL.md 格式）+ ROAD-3（插件 manifest）。落地形态参考设计 §8「ClawHub 技能安装 / 上传归档安装」与 §13 CLI `skills`（search/install/workshop）。

---

## 维护约定

- 新条目加到 TODO 对应分类下，ID 用 `类型-序号`（`BUG` / `FEAT` / `TEST` / `OPS` / `REFACTOR` / `ROAD`）
- 开始做就移到 DOING，卡住移到 BLOCKED 并写明卡在什么
- 做完从本文件删除，把变更写进 `CHANGELOG.md`
- 需要展开的写「条目明细」，简单的只留表格行
- **规划（ROAD-\*）是主题级里程碑**，来源是 `docs/archive/design/01-功能点清单.md`；要动手做某条时，先把它拆成具体的 `FEAT-*` / `REFACTOR-*` 条目再开工
- **后续开发只看本文件**：新需求、方案、设计都写在这里——任务条目进对应表格，方案/设计要点写进「条目明细」。不在 `docs/` 下新建文档
- **`docs/archive/` 已冻结**，不再更新，只在追溯历史决策时翻；`docs/` 现行文档仅描述已落地的当前状态
- **本文件是后续开发的唯一入口**，`CHANGELOG.md` 只记已发布的历史
