# ADR-0002：FTS5 trigram 预切词策略（P2-4 记忆全文索引）

## 状态

已采纳（2026-09-10 落地，v0.2.2 发布）

## 背景

`memory.text` 字段在 10 万条记录下纯 LIKE 扫描约 243ms，是记忆检索的性能瓶颈。需要加全文索引。

SQLite FTS5 提供多种分词器，但都不适合直接用：

- **unicode61**（默认）：把整段中文视为一个 token，查「简洁」时得零行，中文召回静默归零。
- **porter**：英文词干提取，对中文无效。
- **trigram**：只支持 ≥3 字词，但上层 tokenize 产出的是 2-gram（相邻两字对），无法命中。

直接换 tokenizer 的路走不通。

## 决策

**不换分词器，换存进去的内容。**

写入 FTS 索引时，先把 `memory.text` 预切成「相邻两字」词流（2-gram）再交给 unicode61：`"简洁代码"` → `"简洁 洁代 代码"`。查询侧用同一函数对查询词做相同编码。由于索引的是超集（2-gram 覆盖所有相邻字对），SQL 里保留 LIKE 复核做最终判定，结果集与纯 LIKE 完全等价。

表结构选型：

- **contentless**：不重复存原文，节省空间（实测节省约 72%，对比存内容的 FTS 表）。
- **detail=none**：不存位置信息，进一步减小索引体积。
- **contentless_delete=1**：支持按 rowid 删除，否则 contentless 表无法删条目。

## 后果

**正面：**

- 10 万条查询从 243ms 降至 1–7ms，提升约 90 倍。
- 结果集与纯 LIKE 完全等价（SQL 层保留 LIKE 复核）。
- 空间占用小（contentless + detail=none）。

**代价（三个坑，已在代码里处理）：**

1. **upsert 需先删后插**：contentless 表不认 rowid 冲突，同 rowid 插两次会累积 token 且 DELETE 清不净，必须先 DELETE 再 INSERT。
2. **rowid 需显式声明**：contentless 表只能靠 rowid 关联原表，而 rowid 只在声明了 `INTEGER PRIMARY KEY` 时 VACUUM 后才稳定。因此 `memory` 表加了显式 `no` 列（`id` 改为 `UNIQUE`）。
3. **排序**：按 `created_at` 排序需要把命中行灌临时 B-tree，比纯 LIKE 还慢；改为按 `no` 逆序，查询在 1–7ms 以内。
