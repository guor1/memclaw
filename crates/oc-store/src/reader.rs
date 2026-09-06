//! 读连接池（P2-1 读写分离，设计 §3.1）。
//!
//! **问题**：此前读命令（`load_transcript` / `search_candidates` / `cron_list` …）
//! 和写一样投递给单写线程串行执行。一次 `memory.text LIKE '%词%'` 全表扫描
//! 会把写线程占住，**所有会话**的落库、记忆写入、cron 记账全部排在它后面。
//! 设计 §3.1 承诺的是「读走短连接或小连接池 + `spawn_blocking`」，实现从未跟上。
//!
//! **修法**：读走本模块——一个小连接池（`Vec<Connection>` + `Mutex`，非 r2d2；
//! 单用户并发低，简单即可），每次读在 `spawn_blocking` 里跑同步 rusqlite。
//! WAL 下读不阻塞写、写不阻塞读，所以读多慢都不再影响写线程。
//!
//! **`PRAGMA query_only`**：池里的连接虽以读写模式打开（WAL 的 `-shm` 机制在
//! 纯只读连接上有开不出来的坑），但建连时置 `query_only=ON`，任何写语句会被
//! SQLite 当场拒绝。唯一可写连接仍只有写线程那一条——这一点由数据库自己把关，
//! 不靠"约定只在这里写 SELECT"。
//!
//! **池空时开短连接**：不阻塞等归还。归还时若池已满则直接关掉，
//! 于是池大小是「常驻上限」而非「并发上限」——突发并发不会互相排队。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::error::{StoreError, StoreResult};

/// 常驻空闲连接上限。超出的短连接用完即关。
///
/// 单用户场景并发读来源有限（主会话历史加载、Lane1 检索、cron 扫描、
/// dreaming、`oc` 各子命令），4 条足够覆盖常态而不至于攒一堆闲连接。
const MAX_IDLE: usize = 4;

/// 读连接池。`clone` 共享同一池。
#[derive(Clone)]
pub struct Reader {
    idle: Arc<Mutex<Vec<Connection>>>,
    db: PathBuf,
}

impl Reader {
    /// 建池。**不预热**——首次读时才开连接，省掉启动开销。
    ///
    /// 这里不跑迁移：迁移由写线程在开放写之前跑完（§3.2），
    /// 读连接只可能在那之后被建出来。
    pub fn new(db: PathBuf) -> Self {
        Self {
            idle: Arc::new(Mutex::new(Vec::new())),
            db,
        }
    }

    /// 在 `spawn_blocking` 上跑一次读。
    ///
    /// `f` 是同步 rusqlite 闭包（与写线程里的 `ops::*` 同款），
    /// 所以 `ops` 模块无需为读写各写一份。
    pub async fn read<T, F>(&self, f: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> StoreResult<T> + Send + 'static,
    {
        let idle = self.idle.clone();
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let conn = match take_idle(&idle) {
                Some(c) => c,
                None => open_read_conn(&db)?,
            };
            let out = f(&conn);
            put_idle(&idle, conn);
            out
        })
        .await
        // JoinError 只可能来自读闭包 panic（rusqlite 用错）或 runtime 关停。
        // 前者是 bug，不该被静默吞成空结果，故显式转成错误。
        .map_err(|e| StoreError::Migration(format!("读任务失败: {e}")))?
    }

    /// 当前空闲连接数（测试与诊断用）。
    pub fn idle_count(&self) -> usize {
        self.idle.lock().map(|v| v.len()).unwrap_or(0)
    }
}

fn take_idle(idle: &Mutex<Vec<Connection>>) -> Option<Connection> {
    // 锁中毒（持锁线程 panic）时不 unwrap 传播：退化成开短连接即可，
    // 读路径不该因为别的读崩过一次就整体不可用。
    idle.lock().ok()?.pop()
}

fn put_idle(idle: &Mutex<Vec<Connection>>, conn: Connection) {
    if let Ok(mut v) = idle.lock() {
        if v.len() < MAX_IDLE {
            v.push(conn);
        }
        // 池满 → conn 在此 drop，连接关闭。
    }
}

fn open_read_conn(db: &PathBuf) -> StoreResult<Connection> {
    let conn = Connection::open(db)?;
    // 只设读侧相关项：busy_timeout 兜住写事务提交瞬间的短锁，
    // query_only 由 SQLite 强制"这条连接不写"。
    // journal_mode/synchronous 是库级持久设置，由写线程负责，这里不碰。
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个真实文件库（内存库无法跨连接共享，见 `Store` 的说明）。
    fn temp_db(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("oc-reader-{}-{tag}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut conn = Connection::open(&p).expect("建库");
        crate::apply_startup_pragmas(&conn).expect("pragma");
        crate::migrate::run_migrations(&mut conn).expect("迁移");
        p
    }

    #[tokio::test]
    async fn reuses_connection_and_caps_idle() {
        let db = temp_db("reuse");
        let r = Reader::new(db.clone());
        assert_eq!(r.idle_count(), 0, "不预热");

        for _ in 0..3 {
            r.read(crate::schema_version).await.unwrap();
        }
        assert_eq!(r.idle_count(), 1, "串行读应复用同一条连接");

        // 并发超过 MAX_IDLE：多余的短连接用完即关，池不无限增长。
        let mut hs = Vec::new();
        for _ in 0..MAX_IDLE * 3 {
            let r2 = r.clone();
            hs.push(tokio::spawn(async move {
                r2.read(|c| {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    crate::schema_version(c)
                })
                .await
            }));
        }
        for h in hs {
            h.await.unwrap().unwrap();
        }
        assert!(r.idle_count() <= MAX_IDLE, "空闲连接不应超过上限");
        let _ = std::fs::remove_file(&db);
    }

    /// `query_only` 必须真的拦住写——否则"唯一写连接"这个不变量只是口头约定。
    #[tokio::test]
    async fn read_conn_rejects_writes() {
        let db = temp_db("readonly");
        let r = Reader::new(db.clone());
        let err = r
            .read(|c| {
                c.execute("INSERT INTO session(id,kind,created_at) VALUES('x','main',0)", [])?;
                Ok(())
            })
            .await;
        assert!(err.is_err(), "读连接上的写应被 SQLite 拒绝，实际成功了");
        let _ = std::fs::remove_file(&db);
    }
}
