//! 会话注册表（多会话支持）。
//!
//! 把"单会话单车道"升级为**每会话一条车道**：按 [`SessionId`] 懒创建
//! session actor。会话间并发、会话内串行（保留 §10 防卡死语义）。
//!
//! 注册表持有 spawn 一个 session actor 所需的全部依赖克隆（cfg/provider/
//! events/store），`get_or_spawn` 首次遇到某会话 id 时建 actor 并登记。
//!
//! **不做空闲淘汰**：单用户短期无碍；长期可加 LRU/TTL（留 TODO）。

use std::sync::Arc;

use dashmap::DashMap;
use oc_llm::Provider;
use oc_proto::{Event, SessionId};
use tokio::sync::broadcast;

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
}

impl SessionRegistry {
    /// 新建注册表。`main` 会话在此预建，保证启动即就绪。
    pub fn new(
        cfg: SessionConfig,
        provider: Arc<dyn Provider>,
        events: broadcast::Sender<Event>,
        store: oc_store::Store,
    ) -> Self {
        let reg = Self {
            inner: Arc::new(Inner {
                sessions: DashMap::new(),
                cfg,
                provider,
                events,
                store,
            }),
        };
        // 预建 main（不必等首条消息）。
        reg.get_or_spawn(&SessionId::main());
        reg
    }

    /// 取指定会话的句柄；不存在则懒创建 actor 并登记。
    pub fn get_or_spawn(&self, id: &SessionId) -> SessionHandle {
        if let Some(h) = self.inner.sessions.get(id) {
            return h.clone();
        }
        // entry API：避免并发下重复 spawn（同一 id 只建一个 actor）。
        self.inner
            .sessions
            .entry(id.clone())
            .or_insert_with(|| {
                session::spawn(
                    id.clone(),
                    self.inner.cfg.clone(),
                    Arc::clone(&self.inner.provider),
                    self.inner.events.clone(),
                    self.inner.store.clone(),
                )
            })
            .clone()
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
