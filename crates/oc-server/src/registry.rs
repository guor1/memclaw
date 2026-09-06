//! 会话注册表（多会话支持）。
//!
//! 把"单会话单车道"升级为**每会话一条车道**：按 [`SessionId`] 懒创建
//! session actor。会话间并发、会话内串行（保留 §10 防卡死语义）。
//!
//! 注册表持有 spawn 一个 session actor 所需的全部依赖克隆（cfg/provider/
//! events/store），`get_or_spawn` 首次遇到某会话 id 时建 actor 并登记。
//!
//! **空闲淘汰**（P2-3）：[`SessionRegistry::evict_idle`] 由心跳 tick 调用，把闲置
//! 超阈值的子会话 actor 收掉。淘汰是安全的，因为**会话状态不在 actor 里**——
//! transcript / 记忆 / reset 起点全在 SQLite，actor 只持有「当前这一轮」的车道状态
//! （队列、活跃 run 的 cancel token、sink）。因此闲置 actor 重建后行为与未淘汰一致，
//! 见 `gc.rs` 的 `evicted_session_respawns_with_history`。
//!
//! `main` 永不淘汰：它是常驻主会话，淘汰它只省一个 actor，却让下一条消息多等一次
//! 重建 + `ensure_session`。

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use oc_llm::Provider;
use oc_proto::{Event, SessionId};
use tokio::sync::broadcast;
use tracing::debug;

use crate::session::{self, SessionConfig, SessionHandle};

/// 会话注册表：`SessionId → SessionHandle`，懒创建。
#[derive(Clone)]
pub struct SessionRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    sessions: DashMap<SessionId, SessionHandle>,
    cfg: SessionConfig,
    provider: Arc<dyn Provider>,
    events: broadcast::Sender<Event>,
    store: oc_store::Store,
    diag: crate::diag::DiagRegistry,
}

impl SessionRegistry {
    /// 新建注册表。`main` 会话在此预建，保证启动即就绪。
    pub fn new(
        cfg: SessionConfig,
        provider: Arc<dyn Provider>,
        events: broadcast::Sender<Event>,
        store: oc_store::Store,
        diag: crate::diag::DiagRegistry,
    ) -> Self {
        let reg = Self {
            inner: Arc::new(Inner {
                sessions: DashMap::new(),
                cfg,
                provider,
                events,
                store,
                diag,
            }),
        };
        // 预建 main（不必等首条消息）。
        reg.get_or_spawn(&SessionId::main());
        reg
    }

    /// 取指定会话的句柄；不存在**或已失效**则建 actor 并登记。
    ///
    /// 「已失效」= actor 已停（空闲淘汰退出，或极端情况下自身终止）。此时必须换一个
    /// 新 actor，否则登记表里那条死句柄会让该会话此后**永久**不可用：`submit()` 恒
    /// 返回 `None`，dispatch 把它报成「队列已满」，而实际上队列空着、只是没人收命令。
    pub fn get_or_spawn(&self, id: &SessionId) -> SessionHandle {
        if let Some(h) = self.inner.sessions.get(id) {
            if !h.is_closed() {
                return h.clone();
            }
            // 死句柄：落到下面的 entry 分支替换掉。此处 borrow 随 if let 作用域结束
            // 而释放，不会与 entry() 抢同一 shard 的锁。
        }
        // entry API：避免并发下重复 spawn（同一 id 只建一个 actor）。
        match self.inner.sessions.entry(id.clone()) {
            dashmap::mapref::entry::Entry::Occupied(mut e) => {
                // 持锁期间复检：并发者可能已经替换过了，那就用它的。
                if e.get().is_closed() {
                    e.insert(self.spawn_actor(id));
                }
                e.get().clone()
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                e.insert(self.spawn_actor(id)).clone()
            }
        }
    }

    /// 起一个 session actor（不登记）。
    fn spawn_actor(&self, id: &SessionId) -> SessionHandle {
        session::spawn(
            id.clone(),
            self.inner.cfg.clone(),
            Arc::clone(&self.inner.provider),
            self.inner.events.clone(),
            self.inner.store.clone(),
            // 淘汰时诊断格位被一并清掉，这里会重建。
            self.inner.diag.for_session(id),
        )
    }

    /// 淘汰闲置超 `idle_after` 的子会话 actor，返回被淘汰的会话 id（P2-3）。
    ///
    /// 判定权在各 actor（见 [`session::SessionCmd::EvictIfIdle`]）：正跑 run、有
    /// 排队轮、或近期有活动的一律留下。`main` 不参与。
    ///
    /// 顺带清掉被淘汰会话的诊断格位——它与 `sessions` 同为 `SessionId` 键，
    /// 不清就只是把无界增长从一处搬到另一处。
    pub async fn evict_idle(&self, idle_after: Duration) -> Vec<SessionId> {
        let main = SessionId::main();
        // 先取句柄快照：探针要 await，不能持 DashMap 的锁跨 await。
        let candidates: Vec<(SessionId, SessionHandle)> = self
            .inner
            .sessions
            .iter()
            .filter(|e| e.key() != &main)
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();

        let mut evicted = Vec::new();
        for (id, handle) in candidates {
            if !handle.evict_if_idle(idle_after).await {
                continue;
            }
            // actor 已在回执前关掉接收端，故此处 `is_closed()` 必为真。用
            // `remove_if` 而不是直接 `remove`：谓词在 shard 锁内求值，保证不会
            // 误删「探针返回后、并发 get_or_spawn 刚放进去」的那个新 actor。
            let removed = self.inner.sessions.remove_if(&id, |_, h| h.is_closed()).is_some();
            if removed {
                self.inner.diag.forget(&id);
                debug!(session = %id, "空闲会话已从注册表移除");
                evicted.push(id);
            }
        }
        evicted
    }

    /// 当前登记的会话 actor 数（含 main）。可观测/测试用。
    pub fn len(&self) -> usize {
        self.inner.sessions.len()
    }

    /// 注册表是否为空。`main` 在 `new()` 里预建，故常态为 false。
    pub fn is_empty(&self) -> bool {
        self.inner.sessions.is_empty()
    }

    /// 会话配置（注册表持有的那份，各 actor 由它克隆而来）。
    ///
    /// 给不经过 actor 的路径用——如 `session.reset` 的 episodic 沉淀需要
    /// `max_history_entries` 来限定拉取范围。
    pub fn cfg(&self) -> &SessionConfig {
        &self.inner.cfg
    }

    /// 向所有活跃会话广播中止请求。run_id 全局唯一，各 actor 只中止匹配的活跃
    /// run，故对无关会话是 no-op。空 run_id 会中止各会话的活跃 run（见 session actor）。
    pub async fn abort_all(&self, run_id: oc_proto::RunId, hard: bool) {
        let handles: Vec<SessionHandle> =
            self.inner.sessions.iter().map(|e| e.value().clone()).collect();
        for h in handles {
            h.abort(run_id.clone(), hard).await;
        }
    }

    /// 对所有活跃会话触发卡死诊断扫描（心跳 tick 调用）。
    pub async fn health_scan_all(&self) {
        // 收集句柄快照后再 await，避免持锁跨 await。
        let handles: Vec<SessionHandle> =
            self.inner.sessions.iter().map(|e| e.value().clone()).collect();
        for h in handles {
            h.health_scan().await;
        }
    }
}
