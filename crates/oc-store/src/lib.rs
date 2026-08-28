//! oc 单库存储（设计 §3）。
//!
//! 单库 `oc.sqlite` 装全部；schema 版本化 + 前向迁移；WAL；单写线程。
//! **store 只做持久化与查询执行，不做策略判定**（策略在 oc-core）。
//!
//! M1 落地：建库、迁移、WAL/PRAGMA、`user_version` 报告。
//! 单写线程 actor 与完整 Store trait 在 M2+ 补齐。

pub mod error;
pub mod migrate;
pub mod schema;

pub use error::{StoreError, StoreResult};

use std::path::Path;

use rusqlite::Connection;

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
fn apply_startup_pragmas(conn: &Connection) -> StoreResult<()> {
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
}
