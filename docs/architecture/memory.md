# 记忆系统

oc 的记忆分三层，靠两条检索通道和一个夜间巩固过程运转。核心约束：**记忆失败绝不阻塞主会话回复**。

## 三层 tier

| tier | 来源 | 是否自动注入 | 说明 |
|---|---|---|---|
| `curated` | 用户显式「记住…」，或 dreaming 从 episodic 晋升 | ✅ 每轮按相关性注入 | 长期核心记忆，同时渲染进 `MEMORY.md` |
| `episodic` | 会话压缩 / reset 前自动沉淀 | ❌ | 候选池，等 dreaming 判定是否晋升 |
| `working` | 当前会话 transcript | — | 存在 `session` 表，不属于记忆表 |

## provenance：抗投毒分类

每条记忆带 `origin` 列，决定它能否被巩固进长期记忆：

| origin | 来源 | 能否被 dreaming 巩固 |
|---|---|---|
| `Owner` | 用户显式「记住…」 | ✅ |
| `Agent` | 主会话 agent 从对话推断 | ✅ |
| `Untrusted` | `web_fetch` / `web_search` 抓回的内容 | ❌ 结构性排除 |
| `System` | 无法判定来源时的保守兜底 | ❌ 结构性排除 |

**绝不默认 Owner**。分类逻辑是 `oc-core` 里的纯函数 `classify_origin`，dreaming 的门 2 结构性排除 `Untrusted` 和 `System`——这是防止「让模型读一个网页就能往长期记忆里写东西」的关键隔离。

## Lane1：入站检索（延迟敏感，零模型调用）

每条用户消息到达时并行跑三件事，各自套 `tokio::time::timeout`，任何一步超时或失败就跳过它继续回复：

```
用户消息到达
  ├─ store 粗筛候选 → core::memory::rank(now) → 取 top N
  ├─ core::memory::trigger_prefilter → 命中 curated（≥ trigger_threshold，≤ max_per_turn）
  └─ core::memory::intent_prefilter + proactive::allow_fire → 命中 standing intent
       ↓
  注入为隐藏上下文（仅 curated tier 自动注入）
       ↓
  core::prompt::render 组装 → 正常 agent 轮
```

排名公式带半衰期（`halflife_days`，默认 30 天），越久未被访问的记忆权重越低。命中的注入项会置 `injected_mark`，防止召回环重复抽取同一条。

Lane1 全程**不调模型**，这是它能放在延迟敏感路径上的前提。

## Lane2：升级检索（跨会话回忆）

只在「用户有显式召回意图」且「Lane1 无强命中」时触发。server 起一个隔离子会话（`SessionKind::Lane2`）执行跨会话检索，结果以 `Agent` origin 回注主会话，不自动持久化为 `Owner`。

隔离子会话不占用 main 车道。

## episodic 沉淀

会话 `compact` 或 `reset` 之前，把「值得记住」的内容 flush 成 episodic 候选。

**提取策略**（`core::extract_episode_candidates`，纯函数）：
- user + assistant **配对**成一条——记住的是「问了 X 答了 Y」这个完整情节，不是半句话
- 无配对的单条按更高字数门槛保留
- `tool` / `system` 消息跳过（工具输出不是人际情节）
- 寒暄用**长度**过滤（成对合计 < 12 字即丢），不维护寒暄词表——词表永远漏，且会误伤「谢谢，那问题解决了吗」这类有内容的话
- 「记住…」开头的在提取阶段就排除，否则同一事实会在 curated 和 episodic 各存一份，被 dreaming 重复巩固

**两条触发路径**：
- **compact**：在调摘要模型**之前**沉淀，只沉淀进摘要区的那部分。放在模型调用之后的话，摘要失败会连沉淀一起丢——而这批历史正要被有损摘要取代，这是它们进入长期记忆的最后机会。
- **reset**：`flush_before_reset` 由 dispatch handler 调用，不经 session actor、不占车道（reset 只改 `sessions.reset_at` 一列）。

id 用内容哈希（`mem-{hash}`），重复 flush 同一段历史会 upsert 回同一行，幂等。失败只告警，不阻塞 compact / reset 本身。

## Dreaming：夜间巩固

挂在心跳 tick 上，空闲/夜间触发。

```
门 1：core::dreaming_gate 确定性排名门（分数 / 访问频次 / 时间窗）
门 2：结构性排除 origin ∈ {Untrusted, System}
      ↓ 双门通过
巩固模型轮：重写 MEMORY.md
      ↓
写安全（乐观并发）：
  读文件算 content_hash
    → 模型生成新内容
    → 落盘前再读一次算 hash
    → hash 未变：同目录 .tmp + 原子 rename 覆盖
    → hash 变了（用户手改过）：退化为 append，不覆盖
```

判定纯在 `oc-core`（可确定性单测），文件与 DB 读写在 store，调度在 server。

**几处写安全细节**：
- 巩固 prompt 把**既有内容和新条目一起**给模型，令其合并而非只看新条目——否则重写会丢历史记忆
- 模型空输出或超时 → 保持原文件不变，绝不清空用户的长期记忆
- 原子写用同目录 `.tmp` + rename（跨盘 rename 不保证原子）；Windows 上 rename 不能覆盖已存在文件，所以先删目标
- 系统提示词明确禁止编造：夜间无人监督的重写若掺入幻觉，会污染每轮都注入的 curated 核心

`MEMORY.md` 路径由 CLI 经 `SessionConfig::soul_dir` 传入。为 `None` 时（测试 / 内存态）只做 DB 内 tier 提升，完全不碰文件——server 自身不解析 `OC_HOME`。

## 检索性能

`memory.text` 上有 FTS5 全文索引，10 万条量级查询在 1–7ms（对照纯 LIKE 全表扫描 243ms）。中文检索靠 2-gram 预切词绕过 unicode61 分词器的中文缺陷，详见 [ADR-0002](adr/0002-fts5-trigram-tokenization.md)。

## 相关配置

见 [配置参考](../reference/config.md) 的 `[memory]` 段：`halflife_days`、`trigger_threshold`、`trigger_max_per_turn`、`vec`。
