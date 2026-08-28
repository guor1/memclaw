//! 前向迁移（设计 §3.2）。
//!
//! `user_version` PRAGMA 记录版本；启动时逐步升级；**只前不后**（不写 down）。
//! 每个版本步进用事务包裹。

use rusqlite::Connection;

use crate::error::{StoreError, StoreResult};
use crate::schema;

/// 当前目标 schema 版本。新增迁移时 +1 并在 [`step`] 中追加分支。
pub const TARGET_VERSION: u32 = 1;

/// 从当前 `user_version` 前向迁移到 [`TARGET_VERSION`]。
pub fn run_migrations(conn: &mut Connection) -> StoreResult<()> {
    let mut current: u32 = {
        let v: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        v as u32
    };

    if current > TARGET_VERSION {
        return Err(StoreError::Migration(format!(
            "db version {current} is newer than target {TARGET_VERSION}; downgrade unsupported"
        )));
    }

    while current < TARGET_VERSION {
        let next = current + 1;
        let tx = conn.transaction()?;
        step(&tx, next)?;
        // user_version 不能用参数绑定，需内联；next 由内部控制，无注入风险。
        tx.pragma_update(None, "user_version", next as i64)?;
        tx.commit()?;
        current = next;
    }

    Ok(())
}

/// 应用单个版本步进。
fn step(conn: &Connection, version: u32) -> StoreResult<()> {
    match version {
        1 => {
            conn.execute_batch(schema::V1)?;
            #[cfg(feature = "sqlite-vec")]
            conn.execute_batch(schema::V1_VEC)?;
            Ok(())
        }
        other => Err(StoreError::Migration(format!(
            "no migration step defined for version {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent_on_reopen() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        // 再跑一次：已在目标版本，应为 no-op 不报错。
        run_migrations(&mut conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v as u32, TARGET_VERSION);
    }

    #[test]
    fn rejects_future_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", (TARGET_VERSION + 1) as i64)
            .unwrap();
        assert!(run_migrations(&mut conn).is_err());
    }
}
