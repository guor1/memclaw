//! 服务端共享状态（设计 §7.1）。M2 最小集：事件总线 + 幂等缓存。
//!
//! store / providers / tools / scheduler 等随里程碑加入。

use std::sync::Arc;

use dashmap::DashMap;
use oc_proto::{ApprovalId, Event, IdemKey, InputId, MethodOk};
use tokio::sync::{broadcast, oneshot};

use crate::registry::SessionRegistry;

/// 幂等缓存条目（M2 仅缓存 chat.send 的 run_id 结果）。
#[derive(Clone)]
pub struct CachedRes {
    pub ok: MethodOk,
}

/// 待处理审批注册表（可与审批处理器共享）。
pub type ApprovalRegistry = Arc<DashMap<ApprovalId, oneshot::Sender<bool>>>;

/// 待处理用户输入注册表（ask_user 的自由文本回执）。
/// 与 ToolExecutor 共享；`user.reply` 经 state 唤醒等待方。
pub type InputRegistry = Arc<DashMap<InputId, oneshot::Sender<Option<String>>>>;

pub struct ServerState {
    /// 事件广播源。每个连接 `subscribe()` 得到独立接收端。
    event_tx: broadcast::Sender<Event>,
    /// side-effecting 方法的幂等缓存（TTL 由清理策略决定，M2 先不过期）。
    idem: DashMap<IdemKey, CachedRes>,
    /// 会话注册表（每会话一条车道，懒创建）。
    registry: SessionRegistry,
    /// 待处理审批注册表（与审批处理器共享）。
    approvals: ApprovalRegistry,
    /// 待处理用户输入注册表（与 ToolExecutor 共享，ask_user 回执唤醒）。
    inputs: InputRegistry,
    /// 后台任务台账。
    ledger: crate::ledger::TaskLedger,
    /// 持久化句柄（chat.history 查询用）。
    store: oc_store::Store,
    /// 每会话最近一轮真实输入 token（status 查询 + 用量展示）。
    usage: Arc<DashMap<oc_proto::SessionId, u32>>,
    /// 模型上下文窗口（token），供 status/事件展示。
    context_window: u32,
    /// 运行时诊断注册表（`oc debug` 采样）。
    diag: crate::diag::DiagRegistry,
    /// standing intent 的 anti-nagging 默认值（`intent.add` 未指定时用）。
    intent_defaults: crate::session::IntentDefaults,
}

impl ServerState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_tx: broadcast::Sender<Event>,
        registry: SessionRegistry,
        approvals: ApprovalRegistry,
        inputs: InputRegistry,
        ledger: crate::ledger::TaskLedger,
        store: oc_store::Store,
        context_window: u32,
        diag: crate::diag::DiagRegistry,
        intent_defaults: crate::session::IntentDefaults,
    ) -> Self {
        Self {
            event_tx,
            idem: DashMap::new(),
            registry,
            approvals,
            inputs,
            ledger,
            store,
            usage: Arc::new(DashMap::new()),
            context_window,
            diag,
            intent_defaults,
        }
    }

    /// standing intent 的 anti-nagging 默认值（源自配置 `[proactive]`）。
    pub fn intent_defaults(&self) -> &crate::session::IntentDefaults {
        &self.intent_defaults
    }

    /// 模型上下文窗口（token）。
    pub fn context_window(&self) -> u32 {
        self.context_window
    }

    /// 运行时诊断注册表。
    pub fn diag(&self) -> &crate::diag::DiagRegistry {
        &self.diag
    }

    /// 记录某会话最近一轮真实输入 token。
    pub fn set_last_input_tokens(&self, session: oc_proto::SessionId, tokens: u32) {
        self.usage.insert(session, tokens);
    }

    /// 查某会话最近一轮真实输入 token。
    pub fn last_input_tokens(&self, session: &oc_proto::SessionId) -> Option<u32> {
        self.usage.get(session).map(|e| *e)
    }

    /// 后台任务台账。
    pub fn ledger(&self) -> &crate::ledger::TaskLedger {
        &self.ledger
    }

    /// 持久化句柄。
    pub fn store(&self) -> &oc_store::Store {
        &self.store
    }

    /// 收到审批回执，唤醒等待方。
    pub fn resolve_approval(&self, id: &ApprovalId, allow: bool) {
        if let Some((_, tx)) = self.approvals.remove(id) {
            let _ = tx.send(allow);
        }
    }

    /// 收到用户输入回执，唤醒等待方（ask_user）。
    pub fn resolve_input(&self, id: &InputId, text: Option<String>) {
        if let Some((_, tx)) = self.inputs.remove(id) {
            let _ = tx.send(text);
        }
    }

    /// 会话注册表。
    pub fn registry(&self) -> &SessionRegistry {
        &self.registry
    }

    /// 订阅事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.event_tx.subscribe()
    }

    /// 当前事件订阅者数（活跃连接近似）。
    pub fn subscriber_count(&self) -> usize {
        self.event_tx.receiver_count()
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
