//! 运行时诊断状态（可观测性）。
//!
//! session actor 与 run 任务在状态迁移点更新自己会话的一份轻量快照，`oc debug`
//! 通过 `Method::Diagnostics` 采样。目标是让「不回复 / 截断 / 卡死」等时序问题
//! 能被实时快照直接定位，而非靠读代码猜。
//!
//! 并发：底层 `DashMap<SessionId, DiagState>`，每会话一格；actor 与其 run 任务
//! 各持一份 [`SessionDiag`] 句柄（廉价克隆），写各自字段互不阻塞。

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use oc_proto::{RunPhase, RunSnapshot, SessionDiag as SessionDiagView, SessionId};

/// 当前 unix 毫秒。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 单会话运行时状态（内部可变）。
#[derive(Default)]
struct DiagState {
    queue_depth: usize,
    active: Option<RunLive>,
    lane_busy_since: Option<i64>,
    total_runs: u64,
    last_finish_reason: Option<String>,
    last_error: Option<String>,
}

/// 活跃 run 的实时状态。
struct RunLive {
    run_id: String,
    phase: RunPhase,
    started_at: i64,
    last_delta_at: Option<i64>,
    tool_rounds: usize,
    acc_chars: usize,
}

/// 全局诊断注册表。挂在 `ServerState`；`SessionRegistry` 为每个 actor 派生
/// 会话级句柄。
#[derive(Clone)]
pub struct DiagRegistry {
    inner: Arc<DashMap<SessionId, DiagState>>,
    started: Instant,
}

impl DiagRegistry {
    pub fn new() -> Self {
        Self { inner: Arc::new(DashMap::new()), started: Instant::now() }
    }

    /// 为某会话派生一个诊断句柄（actor 与其 run 任务共用）。
    pub fn for_session(&self, session: &SessionId) -> SessionDiag {
        self.inner.entry(session.clone()).or_default();
        SessionDiag { inner: Arc::clone(&self.inner), session: session.clone() }
    }

    /// daemon 运行时长（秒）。
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// 丢弃某会话的诊断格位（其 actor 已被空闲淘汰，P2-3）。
    ///
    /// 不清就白淘汰了：本 map 与 `sessions` 同为 `SessionId` 键，只淘汰 actor
    /// 会把「无界增长」从一处挪到另一处。会话若之后重新活跃，`for_session`
    /// 会重建格位（计数从零起——它统计的是本次 actor 生命周期）。
    pub fn forget(&self, session: &SessionId) {
        self.inner.remove(session);
    }

    /// 采样单个会话为对外视图；该会话尚无 actor（或已被淘汰）时返回 `None`。
    ///
    /// `status` 只关心自己那一格，不必为此扫全表。
    pub fn snapshot_session(&self, session: &SessionId) -> Option<SessionDiagView> {
        self.inner.get(session).map(|e| view(e.key(), e.value()))
    }

    /// 采样全部会话为对外视图。
    pub fn snapshot_sessions(&self) -> Vec<SessionDiagView> {
        let mut out: Vec<SessionDiagView> =
            self.inner.iter().map(|e| view(e.key(), e.value())).collect();
        // 稳定排序：main 优先，其余按 id。
        out.sort_by(|a, b| {
            let ka = (a.session_id != SessionId::main(), a.session_id.as_str().to_string());
            let kb = (b.session_id != SessionId::main(), b.session_id.as_str().to_string());
            ka.cmp(&kb)
        });
        out
    }
}

/// 内部状态 → 对外视图。单会话与全表两条采样路径共用。
fn view(id: &SessionId, s: &DiagState) -> SessionDiagView {
    SessionDiagView {
        session_id: id.clone(),
        queue_depth: s.queue_depth,
        active: s.active.as_ref().map(|r| RunSnapshot {
            run_id: oc_proto::RunId::new(r.run_id.clone()),
            phase: r.phase,
            started_at: r.started_at,
            last_delta_at: r.last_delta_at,
            tool_rounds: r.tool_rounds,
            acc_chars: r.acc_chars,
        }),
        lane_busy_since: s.lane_busy_since,
        total_runs: s.total_runs,
        last_finish_reason: s.last_finish_reason.clone(),
        last_error: s.last_error.clone(),
    }
}

impl Default for DiagRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// 会话级诊断句柄。所有更新方法都是 map 上的一次短写，不跨 await 持锁。
#[derive(Clone)]
pub struct SessionDiag {
    inner: Arc<DashMap<SessionId, DiagState>>,
    session: SessionId,
}

impl SessionDiag {
    fn with<F: FnOnce(&mut DiagState)>(&self, f: F) {
        if let Some(mut e) = self.inner.get_mut(&self.session) {
            f(e.value_mut());
        }
    }

    /// 更新排队深度。
    pub fn set_queue_depth(&self, n: usize) {
        self.with(|s| s.queue_depth = n);
    }

    /// run 起步：登记活跃 run（phase=Starting），占用车道，累加计数。
    pub fn run_start(&self, run_id: &str) {
        let at = now_ms();
        self.with(|s| {
            s.total_runs += 1;
            s.lane_busy_since = Some(at);
            s.active = Some(RunLive {
                run_id: run_id.to_string(),
                phase: RunPhase::Starting,
                started_at: at,
                last_delta_at: None,
                tool_rounds: 0,
                acc_chars: 0,
            });
        });
    }

    /// 切换活跃 run 的阶段（不改其它字段）。
    pub fn set_phase(&self, phase: RunPhase) {
        self.with(|s| {
            if let Some(r) = s.active.as_mut() {
                r.phase = phase;
            }
        });
    }

    /// 收到一次模型 delta：phase→Streaming，刷新 last_delta_at 与累计文本长度。
    pub fn delta_seen(&self, acc_chars: usize) {
        let at = now_ms();
        self.with(|s| {
            if let Some(r) = s.active.as_mut() {
                r.phase = RunPhase::Streaming;
                r.last_delta_at = Some(at);
                r.acc_chars = acc_chars;
            }
        });
    }

    /// 打一次「有进展」的时间戳，不动 phase 与文本长度。
    ///
    /// 给非文本的进展用：reasoning delta、工具调用参数分片、工具轮结束。这些都
    /// 说明 run 活着，但都不该改 phase（阶段由 set_phase 显式管），也不该动
    /// acc_chars（那是可见文本的长度）。
    ///
    /// 卡死诊断读的就是这个时间戳。曾经只有 `delta_seen` 会刷新它，而它只在
    /// `Delta::Text` 上调用——于是模型流式吐一个 15000 字符的工具参数（真机上
    /// 耗时 157 秒）期间，诊断看到的是「一直没动静」，把正常干活的 run 掐了。
    pub fn progress(&self) {
        let at = now_ms();
        self.with(|s| {
            if let Some(r) = s.active.as_mut() {
                r.last_delta_at = Some(at);
            }
        });
    }

    /// 距最近一次进展的秒数。`None` = 无活跃 run，或该 run 还没有过任何进展。
    ///
    /// 后者（`last_delta_at` 为 `None`）的含义是「建流中/等首个 delta」，此时
    /// 调用方应退回 run 总时长来判断——那段确实可能卡在网络上。
    pub fn idle_secs(&self) -> Option<u64> {
        let now = now_ms();
        let at = self
            .inner
            .get(&self.session)?
            .value()
            .active
            .as_ref()
            .and_then(|r| r.last_delta_at)?;
        Some(now.saturating_sub(at).max(0) as u64 / 1000)
    }

    /// 更新工具轮数。
    pub fn set_tool_rounds(&self, n: usize) {
        self.with(|s| {
            if let Some(r) = s.active.as_mut() {
                r.tool_rounds = n;
            }
        });
    }

    /// 采样活跃 run 已调用的工具轮数（日志汇总用）。
    ///
    /// 必须在 `run_done` 清掉 `active` **之前**调用；无活跃 run 时返回 `None`。
    /// 这个值是 run 驱动器一路上用 [`set_tool_rounds`] 打上去的，日志层只读不改，
    /// 无需为「这轮到底调没调工具」去动驱动核心。
    pub fn run_tool_rounds(&self) -> Option<usize> {
        let guard = self.inner.get(&self.session)?;
        guard.value().active.as_ref().map(|r| r.tool_rounds)
    }

    /// run 结束：清活跃 + 释放车道，记录结束原因。
    pub fn run_done(&self, finish_reason: impl Into<String>) {
        self.with(|s| {
            s.active = None;
            s.lane_busy_since = None;
            s.last_finish_reason = Some(finish_reason.into());
        });
    }

    /// compact 开始：占用车道并标记阶段。
    pub fn compact_start(&self) {
        let at = now_ms();
        self.with(|s| {
            s.lane_busy_since = Some(at);
            s.active = Some(RunLive {
                run_id: "compact".to_string(),
                phase: RunPhase::Compacting,
                started_at: at,
                last_delta_at: None,
                tool_rounds: 0,
                acc_chars: 0,
            });
        });
    }

    /// compact 结束：释放车道。
    pub fn compact_done(&self) {
        self.with(|s| {
            s.active = None;
            s.lane_busy_since = None;
        });
    }

    /// 记录一次错误文本。
    pub fn set_error(&self, msg: impl Into<String>) {
        self.with(|s| s.last_error = Some(msg.into()));
    }
}
