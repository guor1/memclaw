//! 服务端共享状态（设计 §7.1）。M2 最小集：事件总线 + 幂等缓存。
//!
//! store / providers / tools / scheduler 等随里程碑加入。

use dashmap::DashMap;
use oc_proto::{Event, IdemKey, MethodOk};
use tokio::sync::broadcast;

use crate::session::SessionHandle;

/// 幂等缓存条目（M2 仅缓存 chat.send 的 run_id 结果）。
#[derive(Clone)]
pub struct CachedRes {
    pub ok: MethodOk,
}

pub struct ServerState {
    /// 事件广播源。每个连接 `subscribe()` 得到独立接收端。
    event_tx: broadcast::Sender<Event>,
    /// side-effecting 方法的幂等缓存（TTL 由清理策略决定，M2 先不过期）。
    idem: DashMap<IdemKey, CachedRes>,
    /// 主会话车道句柄。
    session: SessionHandle,
}

impl ServerState {
    pub fn new(event_tx: broadcast::Sender<Event>, session: SessionHandle) -> Self {
        Self {
            event_tx,
            idem: DashMap::new(),
            session,
        }
    }

    /// 主会话句柄。
    pub fn session(&self) -> &SessionHandle {
        &self.session
    }

    /// 订阅事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.event_tx.subscribe()
    }

    /// 广播一个事件。慢 client 丢事件不影响此处（返回订阅者数量或 0）。
    pub fn emit(&self, event: Event) {
        // broadcast::send 仅在无接收端时 Err，忽略即可。
        let _ = self.event_tx.send(event);
    }

    /// 查幂等缓存。
    pub fn idem_get(&self, key: &IdemKey) -> Option<CachedRes> {
        self.idem.get(key).map(|e| e.clone())
    }

    /// 写幂等缓存。
    pub fn idem_put(&self, key: IdemKey, res: CachedRes) {
        self.idem.insert(key, res);
    }
}
