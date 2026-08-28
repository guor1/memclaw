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
}
