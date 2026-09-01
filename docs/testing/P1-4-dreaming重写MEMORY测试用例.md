# 11 — P1-4 dreaming 重写 MEMORY.md 运行时测试用例

> 编写日期：2026-09-01。对应改动：P1-4「巩固模型轮重写 MEMORY.md」（commit `2948f0b`，
> 见 [下一阶段计划](../plan/下一阶段计划.md) 变更记录 + [04 §11.4](../design/04-详细设计文档.md)）。
> 自动化回归见 `crates/oc-server/tests/dreaming_memory_md.rs`（7 项）+
> `crates/oc-core/src/dreaming.rs`（5 项）。

## ⚠️ 先读这段：为什么本组用例必须手工造数据

**dreaming 目前在真机上不会自然触发**，原因是链条断在上游：

| 环节 | 状态 |
|---|---|
| 谁产生 episodic 记忆？ | ❌ **无人**——设计 [§11.5](../design/04-详细设计文档.md)「压缩/reset 前 flush 为 episodic」**尚未落地** |
| dreaming 取候选 | ✅ 已实现（只取 `tier='episodic'`） |
| 双门判定 + tier 提升 | ✅ 已实现 |
| 巩固模型轮重写 MEMORY.md | ✅ P1-4 已实现 |

所以正常聊天**不会**积累出 episodic 候选，dreaming 每轮扫描都是空转。要验证 P1-4，
必须**手工往库里塞 episodic 记忆**。这不是测试取巧——它准确反映了当前实现状态，
等 §11.5 落地后这些用例可改为「聊够多轮 → 触发压缩 → 自然产生候选」。

**另一个前提**：默认 `heartbeat_secs = 60` 且 `DREAM_EVERY_TICKS = 60`，即
**dreaming 每 60 分钟才跑一轮**。真机验证必须把心跳调快，否则要等一小时。

## 0. 环境准备

沿用 [P0-运行时测试用例 §0](P0-运行时测试用例.md)（OC_HOME 隔离、构建、日志）。
本组额外要求：

### 0.1 把心跳调快（否则等一小时）

编辑 `$OC_HOME/config.toml`：

```toml
[proactive]
heartbeat_secs = 5      # 5 秒一 tick；tick % 60 == 0 → 首轮 dreaming 在 tick=60 即约 5 分钟
```

> 想更快可临时改 `DREAM_EVERY_TICKS`（[lib.rs](../../crates/oc-server/src/lib.rs)）为 1
> 再 `cargo build --release`，则**每 tick 都跑** dreaming（5 秒一轮）。这是最省时间的做法，
> 测完记得改回 60。注意 `tick` 从 1 开始，`tick % 1 == 0` 恒真。

### 0.2 需要真实模型

巩固轮要模型**重写**内容，mock 只会回固定串。用 DeepSeek 等真实 provider。

### 0.3 抓手

```sh
# 日志（关键字）
tail -f "$OC_HOME/logs/oc.log.$(date +%Y-%m-%d)" | grep -E "dreaming|MEMORY"

# 看文件
cat "$OC_HOME/soul/MEMORY.md"

# 看库里的候选与 tier
sqlite3 "$OC_HOME/oc.sqlite" \
  "SELECT id, tier, origin, importance, use_count, text FROM memory;"

# 审计（确认写了哪种动作）
sqlite3 "$OC_HOME/oc.sqlite" \
  "SELECT actor, action, payload FROM audit WHERE actor='dreaming' ORDER BY id DESC LIMIT 5;"
```

关键日志行：
- `dreaming：本轮巩固完成 promoted=N` ← DB 内 tier 提升成功
- `dreaming：MEMORY.md 未被并发修改，整体重写` ← 正常覆盖路径
- `dreaming：MEMORY.md 期间被修改，退化为追加（不覆盖）` ← 并发退化路径
- `dreaming：巩固模型轮无输出，MEMORY.md 保持不变` ← 模型空输出保护
- `dreaming：MEMORY.md 已更新 plan=Overwrite items=N`

### 0.4 造候选的辅助脚本

双门要求（`DreamCfg::default()`）：`tier=episodic`、`origin ∉ {untrusted,system}`、
`importance ≥ 0.5`、`use_count ≥ 2`、**沉淀 ≥3 天**、≤180 天。

`created_at` 是**毫秒**，age 按 `now - created_at/1000` 算，故要塞一个 5 天前的时间戳：

```sh
# 5 天前的毫秒时间戳
FIVE_DAYS_AGO=$(( ($(date +%s) - 5*86400) * 1000 ))

sqlite3 "$OC_HOME/oc.sqlite" <<SQL
INSERT INTO memory(id, tier, origin, text, importance, created_at, use_count, content_hash)
VALUES
 ('ep-1','episodic','agent','用户习惯周五下午做本周复盘',0.8,$FIVE_DAYS_AGO,3,'h1'),
 ('ep-2','episodic','agent','用户偏好用 Rust 写命令行工具',0.9,$FIVE_DAYS_AGO,4,'h2'),
 ('ep-3','episodic','agent','用户每天早上第一件事是看邮件',0.7,$FIVE_DAYS_AGO,2,'h3');
SQL
```

> 也可用 `oc` 聊天时说"记住…"写入 curated，再手工 `UPDATE memory SET tier='episodic'`——
> 但直接 INSERT 更省事，且能精确控制 `use_count` / `created_at`。

---

## 用例索引

| 用例 | 验证点 | 关键性 |
|---|---|---|
| TC-P1-4a | 正常路径：候选 → 模型重写 → MEMORY.md 更新 | ⭐ 核心 |
| TC-P1-4b | 既有内容被合并而非丢弃 | ⭐ 防丢数据 |
| TC-P1-4c | 并发修改 → 退化追加，不覆盖用户手改 | ⭐ 防丢数据 |
| TC-P1-4d | 无候选 → 不跑模型、不碰文件 | 常规 |
| TC-P1-4e | 不可信来源被门2排除 | 常规 |
| TC-P1-4f | 模型异常（断网）→ 文件不被清空 | ⭐ 防丢数据 |
| TC-P1-4g | 无 soul 目录 → 只做 DB 巩固不报错 | 边界 |
| TC-P1-4h | 重写内容无编造（人工核对） | ⭐ 质量 |
| TC-P1-4i | 不留 .tmp 残留文件 | 边界 |

---

## TC-P1-4a — 正常路径：候选 → 模型重写 → MEMORY.md 更新 ⭐

**步骤**：
1. 按 §0.1 调快心跳，§0.2 配真实模型，启动 daemon。
2. 按 §0.4 塞 3 条 episodic 候选。
3. 给 MEMORY.md 一个初始内容：
   ```sh
   printf '# MEMORY.md — curated 核心记忆\n\n（dreaming 巩固会重写这里）\n' \
     > "$OC_HOME/soul/MEMORY.md"
   ```
4. 等 dreaming 触发（看日志 `dreaming：本轮巩固完成`）。
5. 查看结果：
   ```sh
   cat "$OC_HOME/soul/MEMORY.md"
   sqlite3 "$OC_HOME/oc.sqlite" "SELECT id, tier FROM memory;"
   ```

**通过标准**：
- 日志出现 `promoted=3` 与 `MEMORY.md 已更新 plan=Overwrite`。
- 3 条记忆的 `tier` 已变成 `curated`。
- MEMORY.md 里能看到那 3 条事实，按主题分组（`## 小标题` + `- ` 列表）。
- 文件以换行结尾，无 Markdown 代码块包裹（`​```` `）。
- 审计有 `dreaming | rewrite_memory_md`。

**失败信号**：
- 日志只有 `promoted=3` 没有 `MEMORY.md 已更新` → 模型轮没跑；查 `soul_dir` 是否传进来了
  （`OC_HOME` 是否设置、`$OC_HOME/soul/` 是否存在）。
- 文件被清空 → 严重，检查模型是否返回空。

---

## TC-P1-4b — 既有内容被合并而非丢弃 ⭐

**验证**：重写不是「用新条目替换整个文件」，而是**合并**。这是最容易写错的地方。

**步骤**：
1. 先让 MEMORY.md 有一条**与新候选无关**的既有内容：
   ```sh
   printf '## 生活\n- 用户养了一只叫豆豆的猫\n' > "$OC_HOME/soul/MEMORY.md"
   ```
2. 塞新候选（§0.4），等 dreaming 跑完。
3. `cat "$OC_HOME/soul/MEMORY.md"`

**通过标准**：
- **"豆豆"仍在文件里**——既有记忆没被新内容顶掉。
- 新候选的内容也在。

**失败信号**：豆豆消失 → prompt 没带上既有内容，或模型忽略了合并指令。前者查
`build_consolidation_prompt`，后者查系统提示词是否被模型遵守（可换模型再试）。

---

## TC-P1-4c — 并发修改 → 退化追加，不覆盖 ⭐

**验证**：用户在 dreaming 跑模型期间手编 MEMORY.md，改动不能被吞掉。

**难点**：要在「模型开始生成」到「落盘」这个窗口内改文件。真实模型这个窗口有几秒，
够手动操作，但需要卡时机。两种做法：

**做法 A（推荐，靠日志卡点）**：
1. 塞候选，`tail -f` 日志盯着。
2. 一看到 `dreaming：本轮巩固完成` 就立刻（在另一个终端）改文件：
   ```sh
   echo "用户手工加的一行" >> "$OC_HOME/soul/MEMORY.md"
   ```
   （`本轮巩固完成` 打在模型轮**之前**，所以此刻改正好落在窗口里。）
3. 等几秒看结果。

**做法 B（更可控，用慢模型）**：配一个响应慢的模型（或本地大模型），窗口拉长到十几秒，
从容操作。

**通过标准**：
- 日志出现 `MEMORY.md 期间被修改，退化为追加（不覆盖）`。
- 文件里**"用户手工加的一行"仍在**。
- 文末追加了一节，带 `<!-- dreaming 追加（检测到并发修改，未覆盖原文） -->` 标记。
- 审计是 `dreaming | append_memory_md`（不是 `rewrite_memory_md`）。

**失败信号**：手工加的行消失 → 乐观并发失效，**这是丢数据级别的问题**，
立刻查 `decide_write` 的两次 hash 是否真的分别在生成前/落盘前采样。

> 卡不准时机也没关系——自动化用例 `concurrent_modification_falls_back_to_append` 用
> `RacingProvider` 在 `stream_chat` 里改文件，确定性地覆盖了这条路径。真机这一步是
> 额外确认。

---

## TC-P1-4d — 无候选 → 不跑模型、不碰文件

**步骤**：
1. 清空 episodic：`sqlite3 "$OC_HOME/oc.sqlite" "DELETE FROM memory WHERE tier='episodic';"`
2. 记下 MEMORY.md 的修改时间：`stat -c %Y "$OC_HOME/soul/MEMORY.md"`（Windows Git Bash 同样可用）
3. 等 2~3 轮 dreaming。

**通过标准**：
- 日志无 `MEMORY.md 已更新`。
- 文件修改时间**未变**（没有无意义的重写）。
- 不应有任何模型调用（看日志无巩固轮的请求）。

**为何重要**：空转时跑模型是纯浪费 token，且每轮重写会让文件时间戳无意义地变动。

---

## TC-P1-4e — 不可信来源被门2排除

**步骤**：
1. 只塞一条 `origin='untrusted'` 的高分高频候选：
   ```sh
   FIVE_DAYS_AGO=$(( ($(date +%s) - 5*86400) * 1000 ))
   sqlite3 "$OC_HOME/oc.sqlite" <<SQL
   INSERT INTO memory(id,tier,origin,text,importance,created_at,use_count,content_hash)
   VALUES('ep-bad','episodic','untrusted','某网页声称的可疑事实',0.95,$FIVE_DAYS_AGO,9,'hb');
   SQL
   ```
2. 等 dreaming 跑。

**通过标准**：
- `promoted=0`，MEMORY.md **不变**。
- `ep-bad` 的 tier 仍是 `episodic`。

**为何重要**：这是抗投毒的结构门——外部内容不管分多高都不该进 curated（会被每轮注入）。

---

## TC-P1-4f — 模型异常 → 文件不被清空 ⭐

**验证**：模型调用失败/超时/返回空时，绝不能把用户的长期记忆清空。

**步骤**：
1. 让 MEMORY.md 有内容（如 TC-b 的豆豆）。
2. 塞候选。
3. **在 dreaming 触发前断网**（或把 `base_url` 改成一个不可达地址后重启 daemon）。
4. 等 dreaming 跑，看日志。

**通过标准**：
- 日志出现 `巩固模型轮无输出，MEMORY.md 保持不变` 或 `巩固模型调用失败`。
- **MEMORY.md 内容完好无损**。
- DB 内的 tier 提升**照常发生**（`promoted=N` 仍出现）——文件写失败不该回滚 DB 巩固。

**失败信号**：文件被清空或截断 → 严重缺陷。

---

## TC-P1-4g — 无 soul 目录 → 只做 DB 巩固不报错

**步骤**：
1. 临时把 soul 目录挪走：`mv "$OC_HOME/soul" "$OC_HOME/soul.bak"`
2. 塞候选，等 dreaming 跑。

**通过标准**：
- `promoted=N` 正常（DB 内巩固不受影响）。
- daemon **不崩、不报错**（可能有 warn 级日志）。
- 目录会被自动创建并写入文件（`atomic_write` 里有 `create_dir_all`），这是预期行为。

**收尾**：`rm -rf "$OC_HOME/soul"; mv "$OC_HOME/soul.bak" "$OC_HOME/soul"`

---

## TC-P1-4h — 重写内容无编造 ⭐（质量，需人工核对）

**验证**：这是**唯一无法自动化**的一项。夜间无人监督的重写若掺入幻觉，会污染每轮
都注入的 curated 核心，且用户不易察觉。

**步骤**：
1. 塞 3 条**内容明确、彼此无关**的候选（如 §0.4 那三条）。
2. 等 dreaming 重写。
3. **逐条比对** MEMORY.md 与原始 3 条事实。

**通过标准**（人工判断）：
- MEMORY.md 里的每一条都能在原始候选中找到出处。
- **没有**被"合理推断"出来的新事实。比如原文只说"周五做复盘"，
  重写后不该出现"用户是项目经理"或"用户重视流程管理"这类推断。
- 相似条目被合并、矛盾项取更具体的，属正常。

**失败处理**：若模型反复编造，加强 `CONSOLIDATION_SYSTEM_PROMPT`
（[dreaming.rs](../../crates/oc-core/src/dreaming.rs)）的约束措辞，或换更听话的模型。
建议每次换模型后都重跑本项。

---

## TC-P1-4i — 不留 .tmp 残留

**步骤**：跑完上面任意几项后检查：
```sh
ls -la "$OC_HOME/soul/"
```

**通过标准**：
- **没有** `MEMORY.md.tmp` 之类的残留文件（正常路径 rename 走掉，失败路径主动清理）。
- 只有 SOUL.md / USER.md / MEMORY.md / AGENTS.md。

---

## 附：最小验证序列（约 10 分钟）

时间紧就跑这 4 步（覆盖核心 + 三个防丢数据项）：

```sh
# 准备：心跳调 5 秒（或临时把 DREAM_EVERY_TICKS 改成 1 重新构建）
FIVE=$(( ($(date +%s) - 5*86400) * 1000 ))

# 1. TC-b：既有内容 + 新候选 → 应合并（豆豆必须活着）
printf '## 生活\n- 用户养了一只叫豆豆的猫\n' > "$OC_HOME/soul/MEMORY.md"
sqlite3 "$OC_HOME/oc.sqlite" "INSERT INTO memory(id,tier,origin,text,importance,created_at,use_count,content_hash) VALUES('ep-1','episodic','agent','用户习惯周五做复盘',0.8,$FIVE,3,'h1');"
# 等 dreaming → cat MEMORY.md：豆豆 + 周五复盘 都应在

# 2. TC-e：不可信来源不得巩固
sqlite3 "$OC_HOME/oc.sqlite" "INSERT INTO memory(id,tier,origin,text,importance,created_at,use_count,content_hash) VALUES('ep-bad','episodic','untrusted','可疑事实',0.95,$FIVE,9,'hb');"
# 等 dreaming → ep-bad 的 tier 应仍是 episodic

# 3. TC-d：无候选不碰文件
sqlite3 "$OC_HOME/oc.sqlite" "DELETE FROM memory WHERE tier='episodic';"
stat -c %Y "$OC_HOME/soul/MEMORY.md"   # 等两轮后再看，应不变

# 4. TC-i：无残留
ls "$OC_HOME/soul/"                     # 不应有 .tmp
```

---

## 已知限制（不是 bug）

1. **dreaming 在真机上不会自然触发**：无人产生 episodic 记忆（§11.5 未落地）。
   本组用例全靠手工塞数据，这准确反映当前状态。
2. **无每日 token 预算上限**：当前靠 `max_consolidations`（8 条）+ 稀疏触发间接限流。
   真机观察成本后再定，见 [下一阶段计划](../plan/下一阶段计划.md) P1-4 条目。
3. **USER.md 仍未接**：与 MEMORY.md 同为 onboard 建了但无人读写的文件，另开一项。
4. **默认 60 分钟才跑一轮**：`heartbeat_secs=60` × `DREAM_EVERY_TICKS=60`。
   验证时必须调快，否则会误以为功能没生效。

---

## 变更记录

- 2026-09-01 建立本文，对应 P1-4（commit `2948f0b`）。9 个用例，4 项标 ⭐
  （核心路径 + 三个防丢数据项）。特别记录了「dreaming 真机不会自然触发」这个前提，
  以免下次验证时误判为功能失效。
