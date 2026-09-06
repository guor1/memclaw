//! 服务端共享状态（设计 §7.1）。M2 最小集：事件总线 + 幂等缓存。
//!
//! store / providers / tools / scheduler 等随里程碑加入。
//!
//! **内存治理**（P2-3）：本结构里有三张按会话/键无界增长的 map——`idem`（幂等键）、
//! `registry` 的 actor 表、`usage`（每会话最近用量）。三者必须**一起**收，否则
//! 「长期运行内存单调增长」只治了三分之一：淘汰了 actor 但留着它的用量格位和诊断
//! 格位，键还是照攒。统一入口是 [`ServerState::gc_tick`]，由心跳 tick 调用。
//!
//! 不在此列的：`approvals` / `inputs` 在回执时 `remove`，`ledger` 是任务键而非
//! 会话键、有自己的生命周期（本次不动）。

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use oc_proto::{ApprovalId, Event, IdemKey, InputId, MethodOk};
use tokio::sync::{broadcast, oneshot};
use tracing::debug;

use crate::registry::SessionRegistry;

/// 幂等结果的缓存时长。
///
/// 覆盖「client 因超时/断线重试同一请求」的窗口——那是秒到分钟量级，1h 足够宽。
/// 再长就只是占内存：过了这么久的重试，其请求上下文（连接、run 归属）早已不在。
pub const IDEM_TTL: Duration = Duration::from_secs(3600);

/// 会话 actor 的空闲淘汰阈值。
///
/// 24h 是刻意保守的：淘汰只省一个 actor 的内存，误淘汰却要多一次重建 + 历史加载。
/// 真正要防的是「大量随机 session_id 各留一个常驻 actor」的累积，那种键一旦停用
/// 就再也不会回来，等一天再收没有代价。
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// 幂等缓存条目：结果 + 写入时刻（TTL 判定用）。
///
/// 时刻用 `tokio::time::Instant` 而非 `std::time::Instant`：前者在
/// `#[tokio::test(start_paused = true)]` 下可被 `tokio::time::advance` 推动，
/// TTL 到期得以确定性单测，不必真等一小时。生产语义与单调时钟一致。
#[derive(Clone)]
struct IdemEntry {
    ok: MethodOk,
    cached_at: tokio::time::Instant,
}

/// 一次 GC 扫描的结果（日志 + 单测断言用）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// 清掉的过期幂等键数。
    pub idem_expired: usize,
    /// 淘汰的空闲会话 actor 数。
    pub sessions_evicted: usize,
}

impl GcReport {
    /// 本轮是否清掉了东西（日志按需打印）。
    pub fn is_empty(&self) -> bool {
        self.idem_expired == 0 && self.sessions_evicted == 0
    }
}

/// 待处理审批注册表（可与审批处理器共享）。
pub type ApprovalRegistry = Arc<DashMap<ApprovalId, oneshot::Sender<bool>>>;

/// 待处理用户输入注册表（ask_user 的自由文本回执）。
/// 与 ToolExecutor 共享；`user.reply` 经 state 唤醒等待方。
pub type InputRegistry = Arc<DashMap<InputId, oneshot::Sender<Option<String>>>>;

pub struct ServerState {
    /// 事件广播源。每个连接 `subscribe()` 得到独立接收端。
    event_tx: broadcast::Sender<Event>,
    /// side-effecting 方法的幂等缓存。TTL = [`IDEM_TTL`]，由 `gc_tick` 定期清扫。
    idem: DashMap<IdemKey, IdemEntry>,
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

    /// 查幂等缓存；已过 [`IDEM_TTL`] 的条目视为未命中并顺手删除。
    ///
    /// 只靠这里删不够——冷键再也不会被查，会永久占着内存。定期清扫见 `gc_tick`。
    pub fn idem_get(&self, key: &IdemKey) -> Option<MethodOk> {
        let expired = match self.idem.get(key) {
            Some(e) if e.cached_at.elapsed() < IDEM_TTL => return Some(e.ok.clone()),
            Some(_) => true,
            None => false,
        };
        if expired {
            // borrow 已随上面的 match 结束而释放，此处取锁不会自锁。
            self.idem.remove(key);
        }
        None
    }

    /// 写幂等缓存（时间戳由此处打，调用方不必关心 TTL）。
    pub fn idem_put(&self, key: IdemKey, ok: MethodOk) {
        self.idem.insert(key, IdemEntry { ok, cached_at: tokio::time::Instant::now() });
    }

    /// 当前幂等缓存条数（可观测/测试用）。
    pub fn idem_len(&self) -> usize {
        self.idem.len()
    }

    /// 一轮内存 GC：清过期幂等键 + 淘汰空闲会话 actor（P2-3）。由心跳 tick 调用。
    pub async fn gc_tick(&self) -> GcReport {
        self.gc_tick_with(IDEM_TTL, SESSION_IDLE_TIMEOUT).await
    }

    /// [`gc_tick`](Self::gc_tick) 的参数化版本，供单测注入短阈值。
    pub async fn gc_tick_with(&self, idem_ttl: Duration, session_idle_after: Duration) -> GcReport {
        // 幂等键：整表扫一遍。表规模是「TTL 窗口内的 side-effecting 请求数」，
        // 单用户量级下每 tick 全扫的成本远低于维护一个过期堆。
        let before = self.idem.len();
        self.idem.retain(|_, e| e.cached_at.elapsed() < idem_ttl);
        let idem_expired = before.saturating_sub(self.idem.len());

        // 会话 actor：判定权在各 actor 自己（有活跃 run / 排队轮 / 近期活动则留）。
        let evicted = self.registry.evict_idle(session_idle_after).await;
        // 被淘汰会话的用量格位随之清掉——同为 SessionId 键，留着等于没淘汰。
        // （诊断格位由 registry 在移除成功时清，那里才知道移除是否真的发生。）
        for id in &evicted {
            self.usage.remove(id);
        }

        let report = GcReport { idem_expired, sessions_evicted: evicted.len() };
        if !report.is_empty() {
            debug!(
                idem_expired = report.idem_expired,
                sessions_evicted = report.sessions_evicted,
                idem_remaining = self.idem.len(),
                sessions_remaining = self.registry.len(),
                "内存 GC 扫描完成"
            );
        }
        report
    }
}
