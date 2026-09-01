# 07 — P1-2 standing intent 触发链方案

> 制定日期：2026-08-31。权威设计源：[04-详细设计文档.md](../design/04-详细设计文档.md) §4.5/§12.3。
> 计划出处：[05-下一阶段计划.md](下一阶段计划.md) P1-2。
> 本文记录 standing intent（长期待办 / 事件型提醒）触发链的落地方案 + 修改点。

## 1. 这个功能解决什么问题（大白话）

让 oc 能「记住你随口交代的事，等你下次聊到相关话题时主动提醒你」。

- 你说：「以后我聊到出差，提醒我带转换插头。」
- 三天后你说：「我下周要去德国出差……」
- 它自动冒一句：「对了，你上次让我提醒你带转换插头。」

这是**话题触发**（不是定时触发——定时是 cron）：你不知道何时再聊到，但一聊到就该被唤起。

## 2. 现状：有大脑，没接神经

判断逻辑（纯函数）早已就绪，但从未通电：

- `oc_core::memory::intent_prefilter(msg, intents)` — 「这条消息命中了哪几条待办的关键词？」
  （[memory.rs](../../crates/oc-core/src/memory.rs) L224）
- `oc_core::proactive::allow_fire(state, now, cfg)` — 「这条待办现在该提醒吗？」
  anti-nagging：cooldown / budget / expiry（[proactive.rs](../../crates/oc-core/src/proactive.rs) L67）

缺口两处：
1. `standing_intent` 表存在（[schema.rs](../../crates/oc-store/src/schema.rs) L52）但**无任何 ops**——不能写、不能读。
2. 入站消息路径**没人调用**那两个纯函数——没接 session 钩子。

P1-2 = 把这根线接上，让功能真正跑起来。

## 3. zeroclaw 调研结论（2026-08-31）

**zeroclaw 没有 standing intent 这个能力。** 它把相关需求拆成三套彼此独立、无一命中的子系统：

| zeroclaw 子系统 | 做什么 | 为何不是 standing intent |
|---|---|---|
| 向量/BM25 记忆召回（`memory_inject.rs`） | 每轮把「相关知识」注入上下文 | 纯相关性检索，无「待办 / 该不该主动提醒」语义 |
| cron（`cron/types.rs`） | 纯时间触发（Cron/At/Every + `after` 一次性） | 只认时间，无 event/keyword/topic 字段 |
| SOP 事件引擎（`sop/condition.rs`） | 事件触发（webhook/传感器/入站消息） | 条件是 `$.value > 85` 结构化数值比较，**刻意不做自然语言关键词匹配** |

**核心差异**：我们要的「入站自由文本命中话题 → 注入待办提醒」正是 zeroclaw 故意留白的部分。它的记忆注入用嵌入向量而非词法查表；它的事件触发只认结构化条件。所以没有现成代码可抄，但也印证我们的朴素方案（词法预筛 `standing_intent` 表）是合理的差异化设计——一次查表即可，不必上向量。

**三处工程借鉴**（其中两处我们已天然对齐）：

1. **anti-nagging 挂在触发实体上**：zeroclaw 的 cooldown 挂在 SOP 记录本身（`sop.cooldown_secs` + `last_completed`），非靠记忆衰减淘汰。→ 我们的 `allow_fire` 正是从 `standing_intent` 每行读 cooldown/budget/expiry。✅ 已对齐。
2. **注入按来源分流**：zeroclaw 按 `TurnOrigin` 分流避免子任务/cron 误注入。→ 我们的 standing intent 只在 main 会话 `begin_run` 触发，cron/dreaming 走隔离子会话不经此路径。✅ 天然分流。
3. **创建入口用「模型/命令显式创建」而非「从用户消息正则解析」**：zeroclaw 全程靠模型调 `cron_add` 工具创建；其「从对话自动捕获待办」的 `auto_capture`/`suggest_on_query` 是**配置有、实现空**的坑（试过没落地）。→ 直接否掉「Submit 里自动识别待办」这条脆弱路径，改用显式创建。

## 4. 方案

### 4.1 触发链（核心，`oc-server/session.rs`）

在 `begin_run` 里、`load_history` 之后、`start_run` 之前加一步 `intent_scan`：

```
intent_list()                                 // 读全部 standing intent
  └─ core::intent_prefilter(user_msg, intents) // 词法命中 → 命中 id 列表
       └─ 对每条命中：
            建 IntentState{created, last_fired, fired_count}（取自行）
            core::proactive::allow_fire(state, now, 该行的 NagCfg)
              ├─ Allow  → 收集提醒文本 + intent_mark_fired(id, now)
              └─ Deny   → 静默跳过（cooldown/budget/expiry）
  └─ 允许的提醒文本 → 追加进 bootstrap（隐藏上下文注入）
```

- **注入载体**：复用现有 `bootstrap: Vec<MemLine>` 管线（[session.rs](../../crates/oc-server/src/session.rs) `lane1_bootstrap` 同款），文本标注「待办提醒：…」以与记忆区分。**不新开 prompt 段**，省掉动 `prompt.rs`/`PromptInputs`/`RunCtx` 的连锁改动。
- **失败绝不阻塞回复**：与 `lane1_bootstrap` 同规格，任何一步出错返回空、只告警。
- **anti-nagging per-row**：cooldown/budget/expiry 从每条 `standing_intent` 行读（schema 已有列），非全局配置。全局配置只作新建时的默认值。

### 4.2 store 层（`oc-store`）

- `types.rs`：
  - `NewStandingIntent { id, text, keywords: Vec<String>, cooldown_secs, budget, expiry_at }`
  - `StandingIntentRow { id, text, keywords, cooldown_secs, budget, fired_count, last_fired_at, expiry_at, created_at }`
  - keywords 存库为**换行分隔** TEXT（schema `keywords TEXT`），类型层用 `Vec<String>`，ops 层做拼接/切分。
    不用空格分隔：关键词自身可含空格（`"business trip"` 是**一个**词），空格分隔会在取回时
    把它劈成两条，导致单命中 "trip" 就触发——比用户指定的宽得多。见 §4.5 的第二个 bug。
- `ops.rs`（照 `cron_*` 写法）：
  - `intent_add` — INSERT，created_at = now。
  - `intent_list` — SELECT 全部，按 created_at。
  - `intent_rm` — DELETE by id，返回是否删到。
  - `intent_mark_fired` — `fired_count += 1, last_fired_at = ?`。
- `writer.rs`：4 个 `WriteCmd` 变体 + 4 个 async 方法 + `run_loop` 分派。
- 回归测试 `intent_crud_roundtrip`（仿 `cron_crud_roundtrip`）。

### 4.3 创建入口（照 cron，显式创建）

- `oc-proto/method.rs`：`IntentAdd(IntentAddParams)` / `IntentList` / `IntentRm(IntentRmParams)` + `MethodOk::IntentAdd{intent_id}` / `IntentList(Vec<IntentSpec>)` + `IntentId` 类型。
- `oc-server/dispatch.rs`：3 个 handler（`handle_intent_add/list/rm`），expiry_days → expiry_at 换算。
- `oc-cli`：`oc intent add/list/rm` 子命令（[main.rs](../../crates/oc-cli/src/main.rs) + `cli_client.rs`），仿 `oc cron`。

> 模型可直接调用的 intent 工具（对标 `cron_add`）属 **P1-5**（cron/intent 一起工具化），不在本次范围。

### 4.4 配置接线

- `SessionConfig` 加 `intent_defaults: IntentDefaults`（**一个结构体而非 4 个平铺字段**——
  测试构造点只需 `Default::default()` 一行，且语义聚合）。
- 从 `ProactiveConfig` 灌入（`provider_setup.rs`）。`intent_cooldown_secs`/`intent_budget`/
  `intent_expiry_days` 三个键此前**是死键**（配置有、无人读），本次真正接上。
- 新增配置键 `intent_max_per_turn`（每轮注入上限，设计 §12.5 ≤3），带 `serde(default)`
  兜底——老 config.toml 缺该键仍可解析，否则用户升级后 daemon 直接起不来。
- `ServerState` 也持一份 `IntentDefaults`：`intent.add` 未指定 anti-nagging 时用它填充
  （只此一处 `ServerState::new` 调用点，接线成本低，故不用硬编码常量）。
- 分工：cooldown/budget/expiry 是**每条待办自己的**（落库在行上，配置值仅作新建默认）；
  max_per_turn 是**每轮全局**上限。

### 4.5 测试

**实际落地的测试**（全绿；全工作区 166 用例无回归）：

- store 单测（[lib.rs](../../crates/oc-store/src/lib.rs)）：
  - `intent_crud_roundtrip`——增/查/记账/删往返，keywords 编解码不丢。
  - `intent_empty_keywords_roundtrip`——空 keywords + 不过期的退化输入不炸。
  - `intent_keyword_with_space_survives_roundtrip`——含空格关键词整条往返（回归第二个 bug）。
- server 集成测试（[standing_intent.rs](../../crates/oc-server/tests/standing_intent.rs)，6 项）：
  1. `matching_topic_injects_reminder_and_marks_fired`——命中注入 + 抬 `fired_count`；
  2. `unrelated_topic_does_not_inject`——无关话题不注入、不记账；
  3. `cooldown_suppresses_second_trigger`——cooldown 内二次命中静默跳过且不记账；
  4. `budget_exhaustion_silences_intent`——用尽 budget 后静默；
  5. `expired_intent_does_not_trigger`——过期不触发；
  6. `per_turn_cap_limits_injection_count`——5 条同词待办只注入 3 条，未注入的不记账。
- core 单测：`intent_max_per_turn_defaults_to_three`——锁默认值本身（core 无 toml 依赖）。
- CLI 单测（[config_loader.rs](../../crates/oc-cli/src/config_loader.rs)）：
  `old_config_without_intent_max_per_turn_still_loads`——老 config.toml 缺新键仍可解析+校验。
  放这里而非 core：TOML 解析与真实加载路径都在 oc-cli，core 是纯策略层不该引 `toml`。
- CLI 单测（[onboard.rs](../../crates/oc-cli/src/onboard.rs)）：onboard 模板与
  `config.example.toml` 均可解析/校验，且两者 `[proactive]` 不漂移。

### 测试抓到的真 bug（已修）

`expired_intent_does_not_trigger` 一次就抓出实现缺陷：store 存**绝对**时间点
`expiry_at`，core 的 `NagCfg` 收**距创建的时长** `expiry_secs`，而 `expiry_secs == 0`
的语义是**永不过期**。我最初用 `(at - created).max(0)` 换算，于是「已过期」（差值 ≤0）
被夹成 0 → 静默翻转成「永不过期」，过期待办反而永远触发。修法：差值 ≤0 直接判过期跳过，
不进 `NagCfg`。**教训**：跨层语义换算时，"哨兵值"（0=不过期）与"钳位"（max(0)）会冲突。

### 复查抓到的第二个 bug（已修）

事后逐行复查 diff 时发现：keywords 落库原用**空格**分隔，而关键词自身可以含空格
（`oc intent add "带插头" "business trip"` 里 `business trip` 是**一个**关键词）。
取回时被 `split_whitespace` 劈成 "business" / "trip" 两条，于是用户只说 "trip"
就会触发——**触发面比用户指定的宽**，属于静默的语义走偏（测试不会自己报错，因为
往返"看起来"没丢字）。修法：改用换行分隔，编码时把关键词内部空白归一为单空格，
保证分隔符唯一。补回归 `intent_keyword_with_space_survives_roundtrip`。
**教训**：选分隔符要看**值域**是否可能包含它，"往返不丢字"不等于"往返不变形"。

## 5. 关键设计决策（自行定，可改）

1. **注入复用 bootstrap 段**，不新开 prompt 段——最小改动面。
2. **创建入口只做 proto+CLI**，不做模型工具（留 P1-5）、不做自动解析（zeroclaw 验证过脆弱）。
3. **匹配用纯词法 contains**，不上向量——朴素够用，与 Lane1 现状一致。
4. **anti-nagging 读每行**，全局配置仅作新建默认。

## 6. 实际改动清单（按 crate）

| crate | 文件 | 改动 |
|---|---|---|
| oc-store | types.rs | +`NewStandingIntent` +`StandingIntentRow` |
| oc-store | ops.rs | +`intent_add/list/rm/mark_fired` + keywords 编解码 + `now_secs` |
| oc-store | writer.rs | +4 `WriteCmd` + 4 async 方法 + run_loop 分派 |
| oc-store | lib.rs | +3 往返测试（含含空格关键词回归） |
| oc-proto | ids.rs | +`IntentId` |
| oc-proto | method.rs | +`IntentAdd/List/Rm` Method + MethodOk + params + `IntentSpec` |
| oc-server | session.rs | **+`intent_scan`（核心触发链）** + `begin_run` 接线 + `IntentDefaults` |
| oc-server | dispatch.rs | +3 handler（含参数校验）+ Method 路由 |
| oc-server | state.rs | +`intent_defaults` 字段/取值器（供 `intent.add` 填默认） |
| oc-server | lib.rs | 导出 `IntentDefaults` + 透传给 `ServerState` |
| oc-server | tests/standing_intent.rs | +6 项集成测试（新文件） |
| oc-server | tests/*.rs（15 个） | 补 `intent_defaults: Default::default()` |
| oc-core | config.rs | +`intent_max_per_turn`（带 serde default）+ 默认值测试（无新依赖） |
| oc-cli | main.rs | +`oc intent add/list/rm` 子命令 |
| oc-cli | cli_client.rs | +`intent_add/list/rm` |
| oc-cli | config_loader.rs | +老配置缺新键仍可加载的回归测试 |
| oc-cli | provider_setup.rs | 灌 `IntentDefaults`（接上此前死键） |
| oc-cli | onboard.rs | 模板 +新键 + 2 项模板校验测试 |
| 根目录 | config.example.toml | +新键 + 注释说明两类语义 |
| docs | 04/05 | 同步落地状态（§4.5(f)、§12.3、P1-2 勾选） |

计 34 个文件改动 + 2 个新增（本文 + 集成测试）。

## 7. 出口标准 ✅ 达成

`oc intent add` 造一条话题待办后，在相关对话里被自动唤起（注入隐藏上下文提醒模型），且
cooldown/budget/expiry 生效防反复打扰——6 项集成测试逐条覆盖。对齐
[05](下一阶段计划.md) P1-2 与 [04 §12.3](../design/04-详细设计文档.md)。

**尚未做（明确留给后续）**：
- 模型可直接调用的 intent 工具（对标 zeroclaw `cron_add`）→ P1-5 与 cron 工具化一起做。
- 「时间型待办编译成 cron」（[04 §12.3](../design/04-详细设计文档.md) 末行）→ 同 P1-5。
- 向量预筛（`trigger_vec` 列已预留）→ 待 sqlite-vec 接入（P2「向量语义检索」）。
- 真机验证：本次仅自动化测试通过，未做真机对话验证（P0/P1-1 都是真机才暴露出问题的，
  建议下轮真机跑一遍：`oc intent add "带转换插头" 出差` 后聊出差看是否自然提起）。

## 变更记录

- 2026-08-31 建立本方案：含 zeroclaw 调研结论（其无 standing intent，功能拆成向量记忆/cron/SOP 三块无一命中）+ 触发链落地方案 + 按 crate 修改点清单。
- 2026-08-31 **实现完成**，本文同步为落地实况：①配置改用 `IntentDefaults` 结构体聚合
  （而非 4 个平铺字段）并真正接上此前的死键；②新增 `intent_max_per_turn` 配置键 + serde
  兜底（老配置不炸）；③测试抓出并修掉 expiry 语义换算 bug（哨兵值 0 与 max(0) 钳位冲突）；
  ④补 onboard/example 配置模板防漂移测试。
- 2026-08-31 **逐行复查全部改动**（34 改 + 2 新），修掉两处「改偏」：
  ①**真 bug**：keywords 原用空格分隔落库，含空格的关键词（`"business trip"`）取回时被劈成
  两条，触发面比用户指定的宽——改换行分隔 + 补回归测试；
  ②**越界依赖**：给 oc-core 加的 dev-dep `toml` 已撤回（core 是纯策略层不该引 TOML），
  该老配置兼容测试移到 oc-cli `config_loader`——TOML 解析与真实加载路径都在那里。
  复查同时确认：15 个测试文件的一行式补字段、`session.rs`/`dispatch.rs` 各自的
  `now_secs`（就近私有小工具，与既有 `now_millis` 同风格）均属必要，未回改。
  全工作区 166 用例通过，无回归；clippy 无新增告警。
