//! oc 单库存储（设计 §3）。
//!
//! 单库 `oc.sqlite` 装全部；schema 版本化 + 前向迁移；WAL；单写线程。
//! **store 只做持久化与查询执行，不做策略判定**（策略在 oc-core）。
//!
//! M1 落地：建库、迁移、WAL/PRAGMA、`user_version` 报告。
//! 单写线程 actor 与完整 Store trait 在 M2+ 补齐。

pub mod error;
pub mod migrate;
pub mod ops;
pub mod schema;
pub mod types;
pub mod writer;

pub use error::{StoreError, StoreResult};
pub use types::*;
pub use writer::Writer;

use std::path::Path;

use rusqlite::Connection;

/// Store 门面：持有单写线程句柄。读写都经写线程串行（单用户下足够）。
#[derive(Clone)]
pub struct Store {
    writer: Writer,
}

impl Store {
    /// 打开磁盘库并启动写线程。
    pub fn open_path(path: std::path::PathBuf) -> StoreResult<Self> {
        Ok(Self { writer: Writer::spawn(Some(path))? })
    }

    /// 打开内存库（测试）。
    pub fn open_memory() -> StoreResult<Self> {
        Ok(Self { writer: Writer::spawn(None)? })
    }

    pub fn writer(&self) -> &Writer {
        &self.writer
    }
}

/// 打开（或创建）数据库，应用启动 PRAGMA 并跑前向迁移。
///
/// 返回一个已就绪的写连接。M2 起将其移入单写线程 actor。
pub fn open<P: AsRef<Path>>(path: P) -> StoreResult<Connection> {
    let mut conn = Connection::open(path)?;
    apply_startup_pragmas(&conn)?;
    migrate::run_migrations(&mut conn)?;
    Ok(conn)
}

/// 打开内存库（测试用）。
pub fn open_in_memory() -> StoreResult<Connection> {
    let mut conn = Connection::open_in_memory()?;
    apply_startup_pragmas(&conn)?;
    migrate::run_migrations(&mut conn)?;
    Ok(conn)
}

/// 启动 PRAGMA（设计 §3.1）。
pub(crate) fn apply_startup_pragmas(conn: &Connection) -> StoreResult<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

/// 读取当前 schema 版本（`user_version` PRAGMA）。供 `oc doctor` 使用。
pub fn schema_version(conn: &Connection) -> StoreResult<u32> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(v as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_migrates_to_target() {
        let conn = open_in_memory().expect("open");
        let v = schema_version(&conn).expect("version");
        assert_eq!(v, migrate::TARGET_VERSION, "should migrate to target");
    }

    #[test]
    fn expected_tables_exist() {
        let conn = open_in_memory().expect("open");
        for t in [
            "session", "entry", "memory", "cron", "standing_intent", "task", "audit", "kv",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .expect("query");
            assert_eq!(count, 1, "table {t} must exist");
        }
    }

    #[tokio::test]
    async fn entry_roundtrip_and_reset() {
        use crate::types::{NewEntry, Role};
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.ensure_session("main".into(), "main".into()).await.unwrap();

        for (role, text) in [(Role::User, "你好"), (Role::Assistant, "在的")] {
            w.append_entry(NewEntry {
                session_id: "main".into(),
                role,
                content: text.into(),
                tokens_est: 2,
            })
            .await
            .unwrap();
        }

        let hist = w.load_transcript("main".into(), 100).await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].content, "你好"); // 正序
        assert_eq!(hist[1].role, Role::Assistant);

        // reset 后历史起点前移，load 应为空。
        w.reset_session("main".into()).await.unwrap();
        let after = w.load_transcript("main".into(), 100).await.unwrap();
        assert!(after.is_empty(), "reset 后上下文应为空");
    }

    #[tokio::test]
    async fn compact_with_summary_replaces_range() {
        use crate::types::{NewEntry, Role};
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.ensure_session("main".into(), "main".into()).await.unwrap();

        // 5 条历史。
        for (role, text) in [
            (Role::User, "第一个问题"),
            (Role::Assistant, "第一个回答"),
            (Role::User, "第二个问题"),
            (Role::Assistant, "第二个回答"),
            (Role::User, "最近的问题"),
        ] {
            w.append_entry(NewEntry {
                session_id: "main".into(),
                role,
                content: text.into(),
                tokens_est: 2,
            })
            .await
            .unwrap();
        }

        // 压缩前 4 条（seq 1..=4）成摘要，保留第 5 条。
        w.compact_with_summary("main".into(), 4, "前两轮讨论了问题一和问题二".into())
            .await
            .unwrap();

        let hist = w.load_transcript("main".into(), 100).await.unwrap();
        // reset_at=4 → 只回 seq>4：摘要 entry(seq6) + 最近一条(seq5)。
        assert_eq!(hist.len(), 2, "应只剩摘要 + 最近一条: {hist:?}");
        assert!(hist.iter().any(|e| e.content.contains("上下文摘要")), "应有摘要 entry");
        assert!(hist.iter().any(|e| e.content.contains("最近的问题")), "最近消息应保留");
        assert!(!hist.iter().any(|e| e.content.contains("第一个问题")), "旧消息应被排除");
    }

    #[tokio::test]
    async fn memory_upsert_and_search() {
        use crate::types::{NewMemory, Origin, Tier};
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.upsert_memory(NewMemory {
            id: "m1".into(),
            tier: Tier::Curated,
            origin: Origin::Owner,
            text: "用户喜欢简洁的回复".into(),
            keywords: Some("简洁 回复".into()),
            importance: 0.8,
            content_hash: "h1".into(),
        })
        .await
        .unwrap();

        let hits = w
            .search_candidates(vec!["简洁".into()], None, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m1");
        assert_eq!(hits[0].origin, Origin::Owner);

        // 不匹配的词返回空。
        let none = w.search_candidates(vec!["登山".into()], None, 10).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn dream_promote_and_audit_chain() {
        use crate::types::{NewMemory, Origin, Tier};
        let store = Store::open_memory().expect("open");
        let w = store.writer();

        // 一条 episodic 沉淀候选。
        w.upsert_memory(NewMemory {
            id: "e1".into(),
            tier: Tier::Episodic,
            origin: Origin::Agent,
            text: "用户常在周五复盘".into(),
            keywords: None,
            importance: 0.7,
            content_hash: "h1".into(),
        })
        .await
        .unwrap();

        // dream_candidates 应取到 episodic。
        let cands = w.dream_candidates(10).await.unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].tier, Tier::Episodic);

        // 巩固 → curated；此后 curated 检索能命中。
        w.promote_memory("e1".into()).await.unwrap();
        let curated = w
            .search_candidates(vec!["周五".into()], Some(Tier::Curated), 10)
            .await
            .unwrap();
        assert_eq!(curated.len(), 1, "巩固后应进入 curated");
        // episodic 池已空。
        assert!(w.dream_candidates(10).await.unwrap().is_empty());

        // 审计链：连写两条，第二条的 hash_prev = 第一条 hash_self。
        w.write_audit("dreaming".into(), "promote".into(), Some("e1".into()))
            .await
            .unwrap();
        w.write_audit("owner".into(), "remember".into(), None)
            .await
            .unwrap();
        // 无直接读 API，此处只验证不报错即通过（链完整性属实现内不变量）。
    }

    #[tokio::test]
    async fn cron_crud_roundtrip() {
        use crate::types::NewCron;
        let store = Store::open_memory().expect("open");
        let w = store.writer();

        w.cron_add(NewCron {
            id: "c1".into(),
            expr: "0 9 * * *".into(),
            prompt: "写周报".into(),
            tz: "UTC".into(),
            next_at: Some(12345),
        })
        .await
        .unwrap();

        let list = w.cron_list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].expr, "0 9 * * *");
        assert!(list[0].enabled);
        assert_eq!(list[0].next_at, Some(12345));

        // 触发后更新。
        w.cron_mark_fired("c1".into(), 20000, Some(99999)).await.unwrap();
        let after = w.cron_list().await.unwrap();
        assert_eq!(after[0].last_fired_at, Some(20000));
        assert_eq!(after[0].next_at, Some(99999));

        // 删除。
        assert!(w.cron_rm("c1".into()).await.unwrap());
        assert!(w.cron_list().await.unwrap().is_empty());
        // 删不存在的返回 false。
        assert!(!w.cron_rm("nope".into()).await.unwrap());
    }

    #[tokio::test]
    async fn intent_crud_roundtrip() {
        use crate::types::NewStandingIntent;
        let store = Store::open_memory().expect("open");
        let w = store.writer();

        w.intent_add(NewStandingIntent {
            id: "i1".into(),
            text: "带转换插头".into(),
            keywords: vec!["出差".into(), "德国".into()],
            cooldown_secs: 86_400,
            budget: 3,
            expiry_at: Some(999_999),
        })
        .await
        .unwrap();

        let list = w.intent_list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].text, "带转换插头");
        // keywords 经空格编码/解码往返不丢。
        assert_eq!(list[0].keywords, vec!["出差".to_string(), "德国".to_string()]);
        assert_eq!(list[0].cooldown_secs, 86_400);
        assert_eq!(list[0].budget, 3);
        assert_eq!(list[0].fired_count, 0, "新建未触发");
        assert!(list[0].last_fired_at.is_none());
        assert_eq!(list[0].expiry_at, Some(999_999));
        assert!(list[0].created_at > 0, "created_at 应为 unix 秒");

        // 触发一次：fired_count 抬升 + last_fired_at 记录。
        w.intent_mark_fired("i1".into(), 12_345).await.unwrap();
        let after = w.intent_list().await.unwrap();
        assert_eq!(after[0].fired_count, 1);
        assert_eq!(after[0].last_fired_at, Some(12_345));

        // 再触发一次累加（budget 判定在 core，store 只记账）。
        w.intent_mark_fired("i1".into(), 23_456).await.unwrap();
        assert_eq!(w.intent_list().await.unwrap()[0].fired_count, 2);

        // 删除。
        assert!(w.intent_rm("i1".into()).await.unwrap());
        assert!(w.intent_list().await.unwrap().is_empty());
        assert!(!w.intent_rm("nope".into()).await.unwrap());
    }

    #[tokio::test]
    async fn intent_empty_keywords_roundtrip() {
        use crate::types::NewStandingIntent;
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        // 空 keywords（编码为空串）+ 不过期（expiry_at=None）不应炸。
        w.intent_add(NewStandingIntent {
            id: "i2".into(),
            text: "无关键词".into(),
            keywords: vec![],
            cooldown_secs: 0,
            budget: 1,
            expiry_at: None,
        })
        .await
        .unwrap();
        let list = w.intent_list().await.unwrap();
        assert!(list[0].keywords.is_empty());
        assert!(list[0].expiry_at.is_none());
    }

    /// 含空格的关键词必须整条往返，不能被劈开。
    ///
    /// 回归：keywords 曾用空格分隔落库，于是 `business trip` 取回时变成两条
    /// （"business" / "trip"），单命中 "trip" 就触发——比用户指定的宽得多。
    #[tokio::test]
    async fn intent_keyword_with_space_survives_roundtrip() {
        use crate::types::NewStandingIntent;
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.intent_add(NewStandingIntent {
            id: "i3".into(),
            text: "带转换插头".into(),
            keywords: vec!["business trip".into(), "德国".into()],
            cooldown_secs: 0,
            budget: 1,
            expiry_at: None,
        })
        .await
        .unwrap();
        let list = w.intent_list().await.unwrap();
        assert_eq!(
            list[0].keywords,
            vec!["business trip".to_string(), "德国".to_string()],
            "含空格的关键词应整条保留，不得被劈成两条"
        );
    }
}
