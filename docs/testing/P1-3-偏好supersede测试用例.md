# 10 — P1-3 偏好 supersede 运行时测试用例

> 编写日期：2026-09-01。对应改动：P1-3「偏好 supersede 接线」落地（commit `53fd329`，
> 见 [05-下一阶段计划.md](../plan/下一阶段计划.md) 变更记录 + [04 §4.5(e)](../design/04-详细设计文档.md)）。
> 目的：验证「同主题的新偏好就地替换旧值」在真机上确实生效，且**不误覆盖**无关记忆。
> 自动化回归见 `crates/oc-server/tests/memory_write.rs`（4 项）、
> `crates/oc-store/src/{lib,migrate}.rs`（5 项）、`crates/oc-core/src/memory.rs`（3 项）。

## 0. 环境准备

沿用 [07-P0运行时测试用例.md §0](P0-运行时测试用例.md)（OC_HOME 隔离、构建启动、日志）。

**本文的特殊之处**：P1-3 **没有 CLI 入口**——偏好通过普通对话说"记住…"写入，
用户侧完全无感。所以验证不能靠命令输出，主要靠两个抓手：

```sh
# 抓手 1：查库（最可靠，直接看 pref_key 与条目数）
sqlite3 "$OC_HOME/oc.sqlite" \
  "SELECT id, pref_key, text FROM memory WHERE tier='curated' ORDER BY created_at;"

# 抓手 2：日志（看 supersede 判定过程）
export RUST_LOG=oc=debug,oc_server=debug
tail -f "$OC_HOME/logs/oc.log.$(date +%Y-%m-%d)" | grep -E "偏好|supersede"
```

关键日志行：
- `偏好就地替换 old=mem-xxx new=mem-yyy key=Some("编辑器")` ← Replace 成功
- `偏好未变化，跳过写入` ← Ignore
- `偏好查询失败，退化为直接写入` ← 降级路径（不该出现）

> 若无 `sqlite3` 命令，可用 `oc memory search <关键词>` 间接观察，但它看不到
> `pref_key` 列，也数不清同主题条目数——**优先装 sqlite3**。

---

## 用例索引

| 用例 | 验证点 | 关键性 |
|---|---|---|
| TC-P1-3a | 同主题冲突 → 就地替换 | ⭐ 核心 |
| TC-P1-3b | 同主题同值 → 忽略不重复 | 常规 |
| TC-P1-3c | 不同主题 → 各自独立不覆盖 | ⭐ 防丢数据 |
| TC-P1-3d | 非偏好记忆 → 走原路径可并存 | ⭐ 防误覆盖 |
| TC-P1-3e | 替换后的偏好能被 Lane1 正确召回 | 端到端闭环 |
| TC-P1-3f | 老库（v1）升级不丢记忆 | ⭐ 升级安全 |
| TC-P1-3g | 词表未覆盖的偏好 → 退回 append | 边界 |
| TC-P1-3h | 审计链记录 supersede | 可追溯 |

---

## TC-P1-3a — 同主题冲突 → 就地替换 ⭐

**验证**：换编辑器后，旧偏好被顶掉，不留两条矛盾记忆。这是 P1-3 的核心价值。

**步骤**：
1. 启动 daemon + TUI。
2. 在 TUI 里说：
   ```
   记住我用 VS Code 写代码
   ```
3. 查库确认写入：
   ```sh
   sqlite3 "$OC_HOME/oc.sqlite" \
     "SELECT id, pref_key, text FROM memory WHERE pref_key='编辑器';"
   ```
   应见 1 行，`pref_key=编辑器`。
4. 再说（模拟换工具）：
   ```
   记住我改用 Neovim 了
   ```
5. 再查同一条 SQL。

**通过标准**：
- 第 5 步**只有 1 行**，text 是"我改用 Neovim 了"。
- 旧的"我用 VS Code 写代码"**已不存在**（`SELECT ... WHERE text LIKE '%VS Code%'` 为空）。
- 日志出现 `偏好就地替换`，且 `old` / `new` 是两个不同 id。

**失败信号**：
- 两行并存 → supersede 未生效，查 `extract_pref_key` 是否命中（日志无"就地替换"）。
- 一行都没有 → 旧的删了新的没写进去（**严重**，说明先写后删的顺序被破坏）。

---

## TC-P1-3b — 同主题同值 → 忽略不重复

**验证**：反复说同一偏好不产生冗余，也不做无用写入。

**步骤**：
1. 说 `记住我用 Neovim`。
2. 再说一遍完全相同的 `记住我用 Neovim`。
3. 查 `WHERE pref_key='编辑器'`。

**通过标准**：
- 只有 1 行。
- 第 2 次的日志出现 `偏好未变化，跳过写入`（Ignore 分支），**不出现**"就地替换"。

---

## TC-P1-3c — 不同主题 → 各自独立不覆盖 ⭐

**验证**：编辑器/操作系统/语言互不干扰。**误覆盖会真的丢用户信息**，这是保守设计的兜底项。

**步骤**：
1. 依次说三句（每句等回复完再说下一句）：
   ```
   记住我用 Neovim
   记住我现在用 macOS
   记住主力语言是 Rust
   ```
2. 查全部偏好：
   ```sh
   sqlite3 "$OC_HOME/oc.sqlite" \
     "SELECT pref_key, text FROM memory WHERE pref_key IS NOT NULL ORDER BY pref_key;"
   ```

**通过标准**：
- **3 行都在**，`pref_key` 分别是 编辑器 / 操作系统 / 编程语言。
- 日志中**没有**"就地替换"（三者主题不同，都该走 Add）。

**失败信号**：
- 少于 3 行 → `extract_pref_key` 把不同主题归到了同一 key（词表有歧义词），
  查日志的 `key=` 字段看实际抽到什么。

---

## TC-P1-3d — 非偏好记忆 → 走原路径可并存 ⭐

**验证**：普通事实性记忆不受 supersede 影响，多条能共存。

**步骤**：
1. 说两句互不相关的非偏好内容：
   ```
   记住周三下午有例会
   记住房东电话是 13800138000
   ```
2. 查非偏好条目：
   ```sh
   sqlite3 "$OC_HOME/oc.sqlite" \
     "SELECT id, pref_key, text FROM memory WHERE tier='curated' AND pref_key IS NULL;"
   ```

**通过标准**：
- **两行都在**，`pref_key` 均为 NULL（空）。
- 日志中无"就地替换"、无"偏好未变化"（两句都没命中词表，走原路径）。

**为何重要**：若这两条被误判成同主题而互相覆盖，用户会丢掉一条真实信息——
比"功能没生效"严重得多。

---

## TC-P1-3e — 替换后的偏好能被 Lane1 正确召回

**验证**：supersede 与既有的记忆注入管线打通，替换后模型看到的是**新**偏好。

**步骤**：
1. 先建旧偏好：`记住我用 VS Code`
2. 替换：`记住我改用 Neovim 了`
3. 开一句会命中该记忆的话（含"编辑器"或"Neovim"等词）：
   ```
   我平时用什么编辑器？
   ```
4. 观察模型回复，并看日志里本轮的系统提示词（`RUST_LOG` 开 debug 可见
   `构建模型请求 shape=...`；或用 `oc memory search 编辑器` 看召回）。

**通过标准**：
- 模型答 **Neovim**，不提 VS Code。
- `oc memory search 编辑器` 只返回 Neovim 那条。

**说明**：这一步需**真实模型**。mock provider 不会真的读注入内容。

---

## TC-P1-3f — 老库（v1）升级不丢记忆 ⭐

**验证**：P1-3 之前建的库升级到 v2 后，既有记忆完好。**这是用户机上真实会走的路径。**

**步骤**：
1. 用 **P1-3 之前的版本**（`git stash` 或 checkout 上一个 commit `8d9fa05`）构建并启动，
   写入几条记忆：
   ```
   记住我用 VS Code
   记住周三有例会
   ```
   停掉 daemon。确认库版本是 1：
   ```sh
   sqlite3 "$OC_HOME/oc.sqlite" "PRAGMA user_version;"   # 应输出 1
   ```
2. 切回 P1-3 版本（`git checkout feat/multi-session`），重新构建启动 daemon。
3. 检查迁移结果：
   ```sh
   sqlite3 "$OC_HOME/oc.sqlite" "PRAGMA user_version;"   # 应输出 2
   sqlite3 "$OC_HOME/oc.sqlite" \
     "SELECT id, pref_key, text FROM memory ORDER BY created_at;"
   ```

**通过标准**：
- `user_version` = 2。
- **步骤 1 写的两条记忆都还在**，文本未变。
- 两条的 `pref_key` 均为 NULL（升级不回填历史数据）。
- daemon 启动无报错。

**失败信号**：任何记忆丢失或 daemon 起不来 → 立即停止，迁移有问题（比功能缺失严重）。

**补充验证**（可选）：升级后再说 `记住我改用 Neovim 了`，此时新写入的会带
`pref_key=编辑器`，但**不会**替换掉步骤 1 那条 NULL 的 VS Code 记录——
因为 `memory_by_pref_key` 只查 `pref_key` 非空的行。这是**预期行为**（不追溯历史），
若希望历史偏好也纳入管理，需要用户重说一次。

---

## TC-P1-3g — 词表未覆盖的偏好 → 退回 append（边界）

**验证**：保守设计的代价与表现——不认识的偏好主题不会被 supersede 管理。

**步骤**：
1. 说一个**词表没有**的偏好类内容（当前词表覆盖：编辑器/操作系统/编程语言/
   回复风格/回复语言/称呼）：
   ```
   记住我喜欢喝美式咖啡
   记住我改喝拿铁了
   ```
2. 查库。

**通过标准**：
- **两行并存**，`pref_key` 均为 NULL。
- 这是**已知且接受的行为**：漏判只是多留一条冗余记忆，不丢信息；
  而误判会删掉真实数据。若这类主题变得重要，扩 `PREF_TOPICS` 词表即可
  （[memory.rs](../../crates/oc-core/src/memory.rs) `PREF_TOPICS`）。

> 记录到这里是为了**明确边界**，避免下次看到"两条咖啡偏好并存"以为是 bug。

---

## TC-P1-3h — 审计链记录 supersede

**验证**：替换动作可追溯。

**步骤**：
1. 触发一次替换（同 TC-P1-3a）。
2. 查审计表：
   ```sh
   sqlite3 "$OC_HOME/oc.sqlite" \
     "SELECT actor, action, payload FROM audit ORDER BY id DESC LIMIT 5;"
   ```

**通过标准**：
- 出现一条 `owner | supersede | mem-<被删的旧id>`。
- 同时有 `owner | remember | mem-<新id>`（新值的写入审计）。

---

## 附：一次跑完的最小验证序列

时间紧时至少跑这 4 步（覆盖核心 + 两个防丢数据项）：

```
1. 记住我用 VS Code        → 查库应 1 行 pref_key=编辑器
2. 记住我改用 Neovim 了     → 查库仍 1 行，内容变 Neovim     [TC-a 核心]
3. 记住我现在用 macOS       → 查库 2 行（编辑器+操作系统）    [TC-c 不覆盖]
4. 记住周三下午有例会       → 查库 3 行（第 3 行 pref_key 空）[TC-d 不误判]
```

一条 SQL 看全貌：
```sh
sqlite3 "$OC_HOME/oc.sqlite" \
  "SELECT COALESCE(pref_key,'(无)') AS 主题, text FROM memory
   WHERE tier='curated' ORDER BY pref_key, created_at;"
```

预期输出：
```
(无)|周三下午有例会
操作系统|我现在用 macOS
编辑器|我改用 Neovim 了
```

---

## 变更记录

- 2026-09-01 建立本文，对应 P1-3（commit `53fd329`）。8 个用例，其中 4 个标 ⭐
  （核心替换 + 两个防误覆盖 + 老库升级）。
