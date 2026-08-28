//! 单写线程 actor（设计 §3.1）。
//!
//! 一个专用 OS 线程持有唯一可写 `Connection`，通过 mpsc 命令队列**串行**执行所有写。
//! 贴合"事务内不 await"：写线程是同步的，事务体内纯 rusqlite 调用。
//! 上层用异步接口投递命令并 `oneshot` 等回执。

use std::path::PathBuf;
use std::thread;

use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::error::{StoreError, StoreResult};
use crate::ops;
use crate::types::{NewEntry, NewMemory};

/// 写命令：每个变更一种，带 oneshot 回执。
pub enum WriteCmd {
    EnsureSession {
        id: String,
        kind: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    AppendEntry {
        entry: NewEntry,
        reply: oneshot::Sender<StoreResult<i64>>,
    },
    ResetSession {
        id: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    UpsertMemory {
        mem: NewMemory,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    TouchMemory {
        id: String,
        at: i64,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    // ── 读命令（单用户下与写共线程串行，简单可靠；将来可拆读连接池）──
    LoadTranscript {
        session_id: String,
        max_entries: i64,
        reply: oneshot::Sender<StoreResult<Vec<crate::types::Entry>>>,
    },
    SearchCandidates {
        query_terms: Vec<String>,
        tier_filter: Option<crate::types::Tier>,
        limit: i64,
        reply: oneshot::Sender<StoreResult<Vec<crate::types::MemoryRow>>>,
    },
    /// 用于优雅关停。
    Shutdown,
}

/// 写线程句柄。
#[derive(Clone)]
pub struct Writer {
    tx: mpsc::UnboundedSender<WriteCmd>,
}

impl Writer {
    /// 启动写线程。`db` 为 None 时用内存库（测试）。
    pub fn spawn(db: Option<PathBuf>) -> StoreResult<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<WriteCmd>();
        // 用 std 线程 + 一个就绪回执，确保建库/迁移成功后才返回。
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<StoreResult<()>>();

        thread::Builder::new()
            .name("oc-store-writer".into())
            .spawn(move || {
                let conn = match open_writer_conn(db) {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                run_loop(conn, &mut rx);
            })
            .map_err(StoreError::Io)?;

        ready_rx
            .recv()
            .map_err(|_| StoreError::Migration("写线程启动失败".into()))??;
        Ok(Self { tx })
    }

    fn send(&self, cmd: WriteCmd) -> StoreResult<()> {
        self.tx
            .send(cmd)
            .map_err(|_| StoreError::Migration("写线程已停止".into()))
    }

    pub async fn ensure_session(&self, id: String, kind: String) -> StoreResult<()> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureSession { id, kind, reply })?;
        rx.await.map_err(|_| StoreError::Migration("写线程无响应".into()))?
    }

    pub async fn append_entry(&self, entry: NewEntry) -> StoreResult<i64> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::AppendEntry { entry, reply })?;
        rx.await.map_err(|_| StoreError::Migration("写线程无响应".into()))?
    }

    pub async fn reset_session(&self, id: String) -> StoreResult<()> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::ResetSession { id, reply })?;
        rx.await.map_err(|_| StoreError::Migration("写线程无响应".into()))?
    }

    pub async fn upsert_memory(&self, mem: NewMemory) -> StoreResult<()> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::UpsertMemory { mem, reply })?;
        rx.await.map_err(|_| StoreError::Migration("写线程无响应".into()))?
    }

    pub async fn touch_memory(&self, id: String, at: i64) -> StoreResult<()> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::TouchMemory { id, at, reply })?;
        rx.await.map_err(|_| StoreError::Migration("写线程无响应".into()))?
    }

    pub async fn load_transcript(
        &self,
        session_id: String,
        max_entries: i64,
    ) -> StoreResult<Vec<crate::types::Entry>> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::LoadTranscript { session_id, max_entries, reply })?;
        rx.await.map_err(|_| StoreError::Migration("写线程无响应".into()))?
    }

    pub async fn search_candidates(
        &self,
        query_terms: Vec<String>,
        tier_filter: Option<crate::types::Tier>,
        limit: i64,
    ) -> StoreResult<Vec<crate::types::MemoryRow>> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::SearchCandidates { query_terms, tier_filter, limit, reply })?;
        rx.await.map_err(|_| StoreError::Migration("写线程无响应".into()))?
    }
}

fn open_writer_conn(db: Option<PathBuf>) -> StoreResult<Connection> {
    let conn = match db {
        Some(path) => Connection::open(path)?,
        None => Connection::open_in_memory()?,
    };
    crate::apply_startup_pragmas(&conn)?;
    let mut conn = conn;
    crate::migrate::run_migrations(&mut conn)?;
    Ok(conn)
}

fn run_loop(conn: Connection, rx: &mut mpsc::UnboundedReceiver<WriteCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            WriteCmd::EnsureSession { id, kind, reply } => {
                let _ = reply.send(ops::ensure_session(&conn, &id, &kind));
            }
            WriteCmd::AppendEntry { entry, reply } => {
                let _ = reply.send(ops::append_entry(&conn, &entry));
            }
            WriteCmd::ResetSession { id, reply } => {
                let _ = reply.send(ops::reset_session(&conn, &id));
            }
            WriteCmd::UpsertMemory { mem, reply } => {
                let _ = reply.send(ops::upsert_memory(&conn, &mem));
            }
            WriteCmd::TouchMemory { id, at, reply } => {
                let _ = reply.send(ops::touch_memory(&conn, &id, at));
            }
            WriteCmd::LoadTranscript { session_id, max_entries, reply } => {
                let _ = reply.send(ops::load_transcript(&conn, &session_id, max_entries));
            }
            WriteCmd::SearchCandidates { query_terms, tier_filter, limit, reply } => {
                let _ = reply.send(ops::search_candidates(&conn, &query_terms, tier_filter, limit));
            }
            WriteCmd::Shutdown => break,
        }
    }
}
