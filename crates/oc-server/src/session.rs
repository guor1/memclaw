//! 主会话车道 actor（设计 §7.3）。
//!
//! 串行车道：同一时刻至多一个活跃 run。通过命令通道接收 submit/abort。
//! 持有活跃 run 的 CancellationToken 以支持 `chat.abort`（M3：中止活跃 run）。
//! panic 隔离：run 主体用 catch_unwind 包裹（设计 §10.3）。

use std::sync::Arc;
use std::time::Duration;

use oc_core::agent::RunOutcome;
use oc_core::queue::{QueuedTurn, RunQueue, SubmitResult};
use oc_llm::Provider;
use oc_proto::{Event, RunId};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::run::{self, RunCtx};

/// 会话配置（从 Config 派生）。
#[derive(Clone)]
pub struct SessionConfig {
    pub model: String,
    pub system_prompt: Option<String>,
    pub idle_timeout: Duration,
    pub run_timeout: Option<Duration>,
    pub queue_cap: usize,
}

/// 发给 session actor 的命令。
pub enum SessionCmd {
    /// 提交一轮用户输入，返回分配的 run_id。
    Submit {
        text: String,
        reply: oneshot::Sender<RunId>,
    },
    /// 中止：hard=先 drain 排队轮再中止活跃（M4 完整）；M3 中止活跃 run。
    Abort { run_id: RunId, hard: bool },
    /// 活跃 run 结束通知（内部）。
    Finished { run_id: RunId, outcome: RunOutcome },
}

/// actor 句柄。
#[derive(Clone)]
pub struct SessionHandle {
    tx: mpsc::Sender<SessionCmd>,
}

impl SessionHandle {
    pub async fn submit(&self, text: String) -> Option<RunId> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(SessionCmd::Submit { text, reply }).await.ok()?;
        rx.await.ok()
    }

    pub async fn abort(&self, run_id: RunId, hard: bool) {
        let _ = self.tx.send(SessionCmd::Abort { run_id, hard }).await;
    }
}

/// 启动 session actor，返回句柄。
pub fn spawn(
    cfg: SessionConfig,
    provider: Arc<dyn Provider>,
    events: broadcast::Sender<Event>,
) -> SessionHandle {
    let (tx, rx) = mpsc::channel(64);
    let handle = SessionHandle { tx: tx.clone() };
    tokio::spawn(actor_loop(cfg, provider, events, tx, rx));
    handle
}

/// 活跃 run 的可中止句柄。
struct ActiveRun {
    run_id: RunId,
    cancel: CancellationToken,
}

async fn actor_loop(
    cfg: SessionConfig,
    provider: Arc<dyn Provider>,
    events: broadcast::Sender<Event>,
    self_tx: mpsc::Sender<SessionCmd>,
    mut rx: mpsc::Receiver<SessionCmd>,
) {
    let mut queue = RunQueue::new(cfg.queue_cap);
    let mut active: Option<ActiveRun> = None;

    while let Some(cmd) = rx.recv().await {
        match cmd {
            SessionCmd::Submit { text, reply } => {
                let run_id = RunId::new(uuid_v7());
                let _ = reply.send(run_id.clone());
                let turn = QueuedTurn {
                    run_id: run_id.to_string(),
                    text,
                };
                match queue.submit(turn) {
                    SubmitResult::Started(t) => {
                        active = Some(start_run(&cfg, &provider, &events, &self_tx, t));
                    }
                    SubmitResult::Queued => { /* 等活跃结束再起 */ }
                    SubmitResult::Rejected => {
                        warn!("队列已满，拒绝新轮");
                    }
                }
            }
            SessionCmd::Abort { run_id, hard } => {
                if hard {
                    let n = queue.drain_pending();
                    if n > 0 {
                        warn!(drained = n, "hard abort：清空排队轮");
                    }
                }
                if let Some(a) = &active {
                    if a.run_id == run_id || run_id.as_str().is_empty() {
                        a.cancel.cancel();
                    }
                }
            }
            SessionCmd::Finished { run_id, outcome } => {
                if active.as_ref().map(|a| &a.run_id) == Some(&run_id) {
                    if !matches!(outcome, RunOutcome::Completed) {
                        warn!(run_id = %run_id, ?outcome, "run 非正常终态");
                    }
                    active = None;
                    // 取下一个排队轮。
                    if let Some(next) = queue.complete_active() {
                        active = Some(start_run(&cfg, &provider, &events, &self_tx, next));
                    }
                }
            }
        }
    }
}

/// 启动一个 run 任务（含 panic 隔离），返回可中止句柄。
fn start_run(
    cfg: &SessionConfig,
    provider: &Arc<dyn Provider>,
    events: &broadcast::Sender<Event>,
    self_tx: &mpsc::Sender<SessionCmd>,
    turn: QueuedTurn,
) -> ActiveRun {
    let cancel = CancellationToken::new();
    let run_id = RunId::new(turn.run_id.clone());
    let ctx = RunCtx {
        run_id: run_id.clone(),
        user_text: turn.text,
        system_prompt: cfg.system_prompt.clone(),
        model: cfg.model.clone(),
        provider: Arc::clone(provider),
        events: events.clone(),
        cancel: cancel.clone(),
        idle_timeout: cfg.idle_timeout,
        run_timeout: cfg.run_timeout,
    };

    let self_tx = self_tx.clone();
    let rid = run_id.clone();
    tokio::spawn(async move {
        // panic 隔离：单 run panic 不拖垮进程（设计 §10.3）。
        let fut = std::panic::AssertUnwindSafe(run::drive(ctx));
        let outcome = match futures_util::FutureExt::catch_unwind(fut).await {
            Ok(o) => o,
            Err(_) => {
                warn!(run_id = %rid, "run panic，已隔离");
                RunOutcome::Panicked
            }
        };
        let _ = self_tx
            .send(SessionCmd::Finished { run_id: rid, outcome })
            .await;
    });

    ActiveRun { run_id, cancel }
}

/// 简易 UUIDv7（避免为此引入额外依赖；server 已有 uuid）。
fn uuid_v7() -> String {
    uuid::Uuid::now_v7().to_string()
}
