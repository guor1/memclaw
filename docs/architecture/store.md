# 存储层

单个 SQLite 文件承载全部持久状态（`~/.oc/oc.sqlite`），WAL 模式。设计取舍是「单一真相 + 单写线程」：不引入外部数据库，用 SQLite 自身的约束机制强制不变量，而不是靠代码约定。

---

## 表清单

| 表 | 内容 |
|---|---|
| `session` | 会话元数据（id、创建时间、`reset_at` 推进起点）|
| `entry` | 对话历史条目，按 `(session_id, seq)` 建索引 |
| `memory` | 分层记忆（text、tier、origin、importance、use_count、pref_key）|
| `memory_fts` | `memory.text` 的 FTS5 虚表（contentless）|
| `memory_vec` | 向量记忆虚表（`sqlite-vec` feature，默认不编译）|
| `cron` | 定时任务（表达式、时区、`next_at`）|
| `standing_intent` | 话题触发式待办（关键词、cooldown、budget、expiry）|
| `task` | 后台任务记录 |
| `audit` | 审计台账（记忆写入、supersede、intent 触发等）|
| `kv` | 杂项键值（schema 版本等）|

---

## 单写线程

`WriterActor` 独占唯一可写连接，所有写操作作为命令投进队列串行执行。好处是不需要在应用层处理写冲突——SQLite 层面根本不存在第二个可写连接。

代价是写队列会排队。这在单用户场景可接受：写操作都是小事务（插一条记忆、更新一个 `next_at`），不存在长事务霸占队列的情况。

## 读写分离

读操作不走写线程队列，而是走独立的读连接池（`reader.rs`）：小池 + `Mutex`，每次读在 `spawn_blocking` 里跑同步 rusqlite。池空时开短连接，归还时池满则关——所以池大小是**常驻上限**而非并发上限，突发并发不排队。

读连接一律置 `PRAGMA query_only=ON`。这不是为了防御性编程，而是让「唯一可写连接是写线程那条」成为**数据库强制**的不变量，而不是「约定这里只写 SELECT」。

**内存库例外**：`:memory:` 数据库每条连接是一个独立库，要共享得用 `cache=shared` URI，而那会关掉 WAL 并引入 `busy_timeout` 管不着的 `SQLITE_LOCKED_SHAREDCACHE`。所以内存库（测试用）的读回落写线程，两条路径共用同一套 `ops::*` 函数，回落只是少了并行，不是另一套实现。生产路径始终有池。

## 写线程健康

写线程内挂 `HealthGuard`（RAII），drop 时翻健康位。用 RAII 而非在循环末尾赋值，是因为 panic 展开**不会**走到函数末尾——而 panic 恰恰是这个机制最想覆盖的情形。

两级探针分工不同：

- `is_alive()`：一次原子读，答「线程还在吗」，可在每个写操作前廉价调用
- `ping()`：投一个 no-op 命令，答「队列还在推进吗」。长事务卡住时线程活着但不动，`is_alive` 单独答不了这一问

写侧死后**读仍可用**（走独立连接池），写操作返回明确的 `WriterDead` 而非静默失败或进程僵死。这是读写分离顺带买到的降级面：写侧瘫痪不等于存储层整体瘫痪。

不做写线程热重启——panic 说明有 bug 或环境异常（DB 损坏、磁盘满），盲目重启可能循环崩溃，明确降级等人工介入更好。

---

## FTS5 全文索引

记忆检索原本是 `text LIKE '%词%'`，前缀通配让任何索引都用不上，只能全表扫。

**现成分词器都不能用**：

- `unicode61` 把一整段中文当**一个** token。索引里存着 `用户喜欢简洁的回复`，查 `简洁` 得零行——中文召回会静默归零
- `trigram` 支持子串匹配，但只对 ≥3 字的词生效，而上层 `tokenize()` 刻意产 2-gram 覆盖中文双字词

**做法是不换分词器，换存进去的东西**：把文本预切成「相邻两字」的词流交给 `unicode61`（它只按空格切开我们给的 token），查询侧用同一个函数编码。索引因此天然是 LIKE 结果的超集。

```
文本  「简洁，别啰嗦」 → 简洁 洁别 别啰 啰嗦
查询词「洁，别」       → 洁别                → 命中
```

窗口**跨标点**取，不先按分隔符切段。若先切段，`简洁，别啰嗦` 只得到 `简洁 / 别啰 / 啰嗦`，而查询词 `洁，别` 编码出的 `洁别` 不在索引里，本该命中的行会被静默漏掉。跨标点让「文本编码」与「查询词编码」在同一套规则下闭合。

**索引不做最终判定**：SQL 里保留原来的 `LIKE` 复核。因为 `detail=none` 不存位置信息，AND 只保证「这些两字窗口都出现过」而非「它们相邻」。这层复核顺带让「索引与表不同步」只会**少召回**、不会返回错的记忆。

**表结构选择**：

- `content=''`（contentless）——原文已在 `memory.text`，不重复存。实测 10 万条时索引占主表 28%，存内容则要 200~300%
- `detail=none`——不存词位，省空间，代价是相邻性判断交给 LIKE 复核
- `contentless_delete=1`——contentless 表默认删不掉行，而记忆会被 supersede 替换和删除

**三个踩过的坑**：

1. contentless 表的 INSERT 不认 rowid 冲突，同 rowid 插两次会**累积** token，且累积的旧 token 连 DELETE 都清不净（只清最后一次那批）——upsert 必须先删后插
2. contentless 表只能靠 rowid 关联回主表，而 SQLite 只保证**声明了** `INTEGER PRIMARY KEY` 的 rowid 在 VACUUM 后不变。`memory` 因此有显式的 `no INTEGER PRIMARY KEY` 列，`id` 改为 UNIQUE 保持等价约束
3. 候选排序用 `no DESC` 而非 `created_at DESC`——按 `created_at` 排要把命中行灌进临时 B-tree，实测 80~140ms 反而比纯 LIKE 还慢；按 rowid 逆序读才是 1~7ms 的快路径

---

## Schema 演进约定

开发阶段**改 DDL 不写迁移步进**，直接删库重建。

这带来一个陷阱：旧库的 `user_version` 已经等于目标值，迁移整个 no-op，但表结构是旧的——第一次写记忆才炸 `no such table: memory_fts`。版本号查不出这个问题，查结构才查得出来。

`oc doctor` 因此有 `check_shape`：校验实际表结构而非版本号，把「结构过旧」从运行时错误提前成启动时的明确删库指引。处置步骤见 [故障排查](../operations/troubleshooting.md)。
