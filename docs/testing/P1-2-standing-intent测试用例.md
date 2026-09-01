# 09 — P1-2 standing intent 运行时测试用例

> 编写日期：2026-08-31。对应改动：P1-2「standing intent 触发链」落地（见
> [05-下一阶段计划.md](../plan/下一阶段计划.md) 变更记录 + [07-P1-2-standing-intent方案.md](../plan/P1-2-standing-intent方案.md)）。
> 目的：给出**手动启动 daemon + TUI** 后，逐项验证 standing intent 的话题触发机制。
> 自动化回归见 `crates/oc-server/tests/standing_intent.rs`（6 项集成测试）+
> `crates/oc-store/src/lib.rs`（3 项 store 往返测试）；本文覆盖「真机跑起来」才能
> 观察到的端到端交互与自然语言触发效果。

## 0. 环境准备

环境准备（OC_HOME 隔离目录、provider 模式、构建启动、日志抓手）与 P0/P1-1 用例**完全一致**，
直接沿用 [07-P0运行时测试用例.md §0](P0-运行时测试用例.md)，此处不重复。

> **本文特别要求**：standing intent 需**真实模型**——mock provider 不会看到隐藏上下文
> 的注入效果（测试用 `CapturingMock` 断言 prompt，但真机要看模型能否自然提起）。
> 与 ask_user 不同，standing intent 不走工具调用，它在 session 启动时注入 bootstrap
> （隐藏上下文），所以每一轮**入站消息**都会触发扫描，无需特殊引导。

---

## 用例索引

| 用例 | 验证点 | 需要 |
|---|---|---|
| TC-P1-2a | intent add/list/rm CLI 基本往返 | daemon + CLI |
| TC-P1-2b | 话题命中 → 模型自然提起 | 真实模型 + TUI |
| TC-P1-2c | 无关话题 → 不触发 | 真实模型 + TUI |
| TC-P1-2d | cooldown 抑制二次触发 | 真实模型 + TUI + oc debug |
| TC-P1-2e | budget 用尽后静默 | 真实模型 + TUI |
| TC-P1-2f | 过期不触发 | 真实模型（可用 --expiry-days=0.01 加速） |
| TC-P1-2g | 含空格关键词整条匹配 | 真实模型 + TUI |
| TC-P1-2h | 每轮注入上限生效 | 真实模型 + TUI |

---

## TC-P1-2a — intent add/list/rm CLI 基本往返 ✅（最基础）

**验证**：`oc intent` 命令增删查往返正常、输出符合预期。

**步骤**：
1. 终端 A 启动 daemon，终端 B 作为 CLI。
2. 添加一条待办：
   ```bash
   oc intent add "带转换插头" 出差 德国
   ```
   观察输出 `已添加话题待办：intent-<uuid>`。
3. 列出所有待办：
   ```bash
   oc intent list
   ```
   观察输出格式：`<id>  触发词:[出差 德国]  已提醒:0/3  上次:从未  «带转换插头»`
4. 删除刚建的待办：
   ```bash
   oc intent rm <上一步看到的 id>
   ```
   观察输出 `已删除。`，再 `oc intent list` 确认为空或只剩其他待办。

**通过标准**：
- 三个命令都能正常返回，无报错。
- `list` 的输出包含 text / keywords / fired_count / budget / last_fired_at 字段，
  格式清晰（触发词用空格连接、已提醒显示分数式）。
- 关键词按原样往返（如 `出差 德国` 是两个词，不是 `出差德国` 一个词）。

**失败特征**：`add` 报 BadRequest（如 text 为空、keywords 为空）；`list` 炸裂或字段错乱；
`rm` 不存在的 id 也报错（应静默成功）。

---

## TC-P1-2b — 话题命中 → 模型自然提起 ✅（核心用例）

**验证**：入站消息命中关键词后，模型在回复中**自然提起**待办内容（无生硬的"待办提醒"字样，
而是融入语境）。

**步骤**：
1. 建一条待办（同 TC-P1-2a）：
   ```bash
   oc intent add "带转换插头" 出差 德国
   ```
2. TUI 里**不主动提「插头」**，而是聊与关键词相关的话题，例如：
   > 我下周要去德国出差，应该准备点什么？
3. 观察模型回复是否**在合理位置**提到「转换插头」（如列在准备清单里、或说"别忘了带转换插头"）。

**通过标准**：
- 模型回复自然提到「转换插头」，读起来像它「知道」你交代过这件事，而非生硬插一句。
- daemon 日志 `grep -i "standing intent"` 可见 `intent：触发注入`、`intent = <id>`、`fired = 1`。
- 再次 `oc intent list` 看到 `已提醒:1/3`，`上次:<unix时间戳>`（从未 → 已触发）。

**失败特征**：
- 模型完全没提插头（可能是注入失败，或模型选择不提——后者不算测试失败，属模型决策；
  可改用更强的引导："列出所有要带的东西"）。
- 模型回复里有生硬的「待办提醒：…」字样泄露到用户可见文本（注入层出错）。

---

## TC-P1-2c — 无关话题 → 不触发

**验证**：入站消息不含关键词时，待办不触发、不记账。

**步骤**：
1. 沿用 TC-P1-2b 的待办（关键词「出差 德国」）。
2. TUI 聊**完全无关**的话题：
   > 今天天气怎么样？
3. 观察模型回复。

**通过标准**：
- 模型**不提**转换插头（若提了属误触发——检查 `intent_prefilter` 逻辑或关键词是否写错）。
- daemon 日志无 `intent：触发注入` 相关条目。
- `oc intent list` 的 `已提醒` 计数**不变**（未记账）。

---

## TC-P1-2d — cooldown 抑制二次触发 🟠

**验证**：cooldown 时长内再次命中同一待办，静默跳过不重复提醒、不记账。

**步骤**：
1. 建一条 **cooldown 极短** 的待办（方便测试）：
   ```bash
   oc intent add "记得喝水" 健康 --cooldown-secs=30
   ```
2. TUI 第一次聊到「健康」，观察模型提起「喝水」，`oc intent list` 确认 `已提醒:1/3`。
3. **立即**（< 30s）再发一条含「健康」的消息：
   > 健康生活方式还包括什么？
4. 观察模型回复。

**通过标准**：
- 第二次**不再提**「喝水」（cooldown 内静默）。
- daemon 日志第二次出现 `intent：命中但拒绝触发 reason=Cooldown`。
- `已提醒` 计数**仍为 1**（未二次记账）。
5. 等 31 秒后再聊「健康」，这次应该能触发（cooldown 过期）。

**失败特征**：cooldown 内仍重复提醒（anti-nagging 失效）；或 cooldown 过后也不触发（判定过严）。

---

## TC-P1-2e — budget 用尽后静默

**验证**：同一待办触发次数达上限后，再次命中话题也不提醒、不记账。

**步骤**：
1. 建一条 **budget=2** 的待办：
   ```bash
   oc intent add "检查邮箱" 工作 --budget=2 --cooldown-secs=1
   ```
   （cooldown 设 1 秒便于快速重复触发）
2. TUI 里连续 3 次聊「工作」话题（每次间隔 > 1s，让 cooldown 过期）：
   - 第 1 次：观察模型提「检查邮箱」，`已提醒:1/2`。
   - 第 2 次：观察模型再提「检查邮箱」，`已提醒:2/2`（用尽 budget）。
   - 第 3 次：观察模型回复。

**通过标准**：
- 第 3 次**不再提**「检查邮箱」（budget 用尽后静默）。
- daemon 日志第 3 次 `reason=BudgetExhausted`。
- `已提醒` 计数**仍为 2**（未溢出）。

---

## TC-P1-2f — 过期不触发

**验证**：待办过期后不再触发，即使话题命中。

**步骤**：
1. 建一条 **极短过期** 的待办（测试用）：
   ```bash
   oc intent add "临时提醒" 测试 --expiry-days=0.001
   ```
   （0.001 天 ≈ 86 秒；或直接等真实过期时间，适合长期验证）
2. **立即**聊「测试」话题，观察能触发。
3. **等 90 秒**后（过期），再聊「测试」。

**通过标准**：
- 过期前能触发；过期后**不触发**。
- daemon 日志过期后 `reason=Expired` 或 `已过期，跳过`（debug 级）。
- `oc intent list` 仍显示该条（过期不自动删除，需手动 `rm`）。

> **注**：`--expiry-days=0` 表示**永不过期**（哨兵值），非「立即过期」。

---

## TC-P1-2g — 含空格关键词整条匹配 ✅（回归第二个 bug）

**验证**：关键词自身含空格时，必须**整条命中**才触发，不能被劈开。

**步骤**：
1. 建一条含空格关键词的待办：
   ```bash
   oc intent add "预定机票" "business trip" "international travel"
   ```
   （`business trip` 和 `international travel` 各是**一个**关键词）
2. TUI 聊「I have a trip next week」（只含 `trip`，不含完整的 `business trip`）。
3. 观察模型回复**不应提**「预定机票」（未命中）。
4. 再聊「I have a business trip next week」（完整匹配 `business trip`）。
5. 观察模型回复**应提**「预定机票」。

**通过标准**：
- 步骤 3 不触发（单独 `trip` 不够）。
- 步骤 5 触发（完整短语匹配）。
- `oc intent list` 显示关键词为 `[business trip international travel]`（两条，非四条）。

**失败特征**：步骤 3 误触发（说明 `business trip` 被劈成 `business` / `trip` 两条，
触发面变宽）——这正是 2026-08-31 复查抓到的 bug，已用换行分隔符修复。

---

## TC-P1-2h — 每轮注入上限生效

**验证**：一条消息同时命中多条待办时，只注入前 N 条（配置 `intent_max_per_turn`，默认 3）。

**步骤**：
1. 建 **5 条** 待办，全部用同一关键词「项目」：
   ```bash
   oc intent add "检查进度" 项目 --budget=10
   oc intent add "更新文档" 项目 --budget=10
   oc intent add "通知客户" 项目 --budget=10
   oc intent add "备份代码" 项目 --budget=10
   oc intent add "开会讨论" 项目 --budget=10
   ```
   （budget 设高防提前用尽）
2. TUI 聊一次「项目」话题：
   > 今天项目有什么要做的？
3. 观察模型回复提到几项。

**通过标准**：
- 模型回复**最多提到 3 项**（配置默认 `intent_max_per_turn=3`）。
- daemon 日志 `达每轮注入上限，其余顺延`（debug 级）。
- `oc intent list` 看到**恰好 3 条** `已提醒` 从 0 抬到 1，另外 2 条仍为 0（未注入的不记账）。

**失败特征**：5 条全部触发（上限失效）；或全部不触发（预筛出错）。

---

## 快速冒烟（P1-2，5 分钟）

启动同 P0/P1-1（真实模型、带工具）。CLI + TUI 里依次：
1. `oc intent add "带转换插头" 出差` → 聊「下周出差」→ 看模型提起（TC-P1-2a + TC-P1-2b）。
2. 聊无关话题「天气」→ 确认不提插头（TC-P1-2c）。
3. `oc intent add "检查邮箱" 工作 --budget=1` → 聊「工作」两次 → 第一次提、第二次不提（TC-P1-2e）。
4. `oc intent add "订机票" "business trip"` → 聊「trip」不触发、聊「business trip」触发（TC-P1-2g）。
5. 全程 `tail -f $OC_HOME/logs/oc.log.*`，确认 `standing intent：触发注入` / `命中但拒绝触发` 日志与预期一致。

## 真机验证的特殊性

与 P0/P1-1 的"功能级"验证（按钮点了有反应、审批能取消）不同，P1-2 的验证重点是
**模型能否自然提起注入的内容**——这取决于：

1. **注入成功** ← 自动化已覆盖（集成测试断言 prompt 里有「待办提醒」）。
2. **模型理解注入** ← 需真机验证。模型可能因以下原因不提：
   - 上下文太长，bootstrap 注入的内容被"淹没"（可调低 `trigger_max_per_turn` 或缩短其他 prompt）。
   - 模型判断当前语境不适合提（如用户问「今天天气」，模型不会突兀地说「对了，你让我提醒你带插头」）。
   - 注入文本的措辞不够引导性（当前是「待办提醒（用户此前交代，现因话题命中而唤起）：…」）。

若真机测出「注入了但模型不提」，**不一定是 bug**——可能需调整注入措辞或引导语，
属**提示词工程**范畴而非代码缺陷。自动化测试只保证「该注入的注入了、该静默的静默了」，
真机测试保证「模型看懂了注入、用户体验自然」。

## 记录模板

| 用例 | 结果 | 备注/日志片段 |
|---|---|---|
| TC-P1-2a CLI 往返 | ☐ pass ☐ fail | |
| TC-P1-2b 话题命中→提起 | ☐ pass ☐ fail | |
| TC-P1-2c 无关话题→不触发 | ☐ pass ☐ fail | |
| TC-P1-2d cooldown 抑制 | ☐ pass ☐ fail | |
| TC-P1-2e budget 用尽 | ☐ pass ☐ fail | |
| TC-P1-2f 过期不触发 | ☐ pass ☐ fail | |
| TC-P1-2g 含空格关键词 | ☐ pass ☐ fail | |
| TC-P1-2h 每轮上限 | ☐ pass ☐ fail | |
