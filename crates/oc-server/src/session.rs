//! 主会话车道 actor（设计 §7.3）。
//!
//! 串行车道：同一时刻至多一个活跃 run。通过命令通道接收 submit/abort。
//! 持有活跃 run 的 CancellationToken 以支持 `chat.abort`（M3：中止活跃 run）。
//! panic 隔离：run 主体用 catch_unwind 包裹（设计 §10.3）。

use std::sync::Arc;
use std::time::Duration;

use oc_core::agent::RunOutcome;
use oc_core::queue::{diagnose, QueuedTurn, RunHealth, RunQueue, SubmitResult};
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
    /// 工具执行器（None = 纯对话，无工具）。
    pub tools: Option<crate::tools_bridge::ToolExecutor>,
    /// 卡死诊断警告阈值（秒）。超过则标记 long_running。
    pub warn_secs: u64,
    /// 卡死 abort 下限（秒）。达到 abort 条件才释放车道（设计 §10.1）。
    pub abort_min_secs: u64,
    /// 加载历史的最大条数（一次拉取上限）。
    pub max_history_entries: i64,
    /// 历史 token 预算（超出则丢弃更早的消息）。
    pub history_token_budget: i64,
    /// SOUL.md 人格文本（每轮由 oc-core::prompt 确定性组装进系统提示词）。
    pub soul: String,
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
    /// 卡死诊断扫描（由心跳 tick 触发）：检查活跃 run 是否卡死。
    HealthScan,
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

    /// 触发一次卡死诊断扫描（心跳 tick 调用）。
    pub async fn health_scan(&self) {
        let _ = self.tx.send(SessionCmd::HealthScan).await;
    }
}

/// 启动 session actor，返回句柄。
pub fn spawn(
    cfg: SessionConfig,
    provider: Arc<dyn Provider>,
    events: broadcast::Sender<Event>,
    store: oc_store::Store,
) -> SessionHandle {
    let (tx, rx) = mpsc::channel(64);
    let handle = SessionHandle { tx: tx.clone() };
    tokio::spawn(actor_loop(cfg, provider, events, tx, rx, store));
    handle
}

/// 活跃 run 的可中止句柄。
struct ActiveRun {
    run_id: RunId,
    cancel: CancellationToken,
    started_at: std::time::Instant,
}

async fn actor_loop(
    cfg: SessionConfig,
    provider: Arc<dyn Provider>,
    events: broadcast::Sender<Event>,
    self_tx: mpsc::Sender<SessionCmd>,
    mut rx: mpsc::Receiver<SessionCmd>,
    store: oc_store::Store,
) {
    let mut queue = RunQueue::new(cfg.queue_cap);
    let mut active: Option<ActiveRun> = None;

    // 确保主会话存在。
    if let Err(e) = store
        .writer()
        .ensure_session("main".into(), "main".into())
        .await
    {
        warn!(error = %e, "创建主会话失败");
    }

    while let Some(cmd) = rx.recv().await {
        match cmd {
            SessionCmd::Submit { text, reply } => {
                let run_id = RunId::new(uuid_v7());
                let _ = reply.send(run_id.clone());

                // 落库用户消息（重启不失忆）。
                let est = estimate_tokens(&text);
                if let Err(e) = store
                    .writer()
                    .append_entry(oc_store::NewEntry {
                        session_id: "main".into(),
                        role: oc_store::Role::User,
                        content: text.clone(),
                        tokens_est: est,
                    })
                    .await
                {
                    warn!(error = %e, "落库用户消息失败");
                }

                let turn = QueuedTurn {
                    run_id: run_id.to_string(),
                    text,
                };
                match queue.submit(turn) {
                    SubmitResult::Started(t) => {
                        let history = load_history(&store, &cfg).await;
                        active = Some(start_run(&cfg, &provider, &events, &self_tx, &store, t, history));
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
                        let history = load_history(&store, &cfg).await;
                        active = Some(start_run(&cfg, &provider, &events, &self_tx, &store, next, history));
                    }
                }
            }
            SessionCmd::HealthScan => {
                if let Some(a) = &active {
                    let elapsed = a.started_at.elapsed().as_secs();
                    match diagnose(elapsed, cfg.warn_secs, cfg.abort_min_secs) {
                        RunHealth::Stuck => {
                            warn!(
                                run_id = %a.run_id,
                                elapsed,
                                "卡死诊断：run 卡死，中止以释放车道"
                            );
                            a.cancel.cancel();
                        }
                        RunHealth::LongRunning => {
                            warn!(run_id = %a.run_id, elapsed, "run 慢(long_running)，暂不中止");
                        }
                        RunHealth::Healthy => {}
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
    store: &oc_store::Store,
    turn: QueuedTurn,
    history: Vec<oc_llm::Message>,
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
        tools: cfg.tools.clone(),
        store: store.clone(),
        history,
        soul: cfg.soul.clone(),
        // 第 4/5 段接入 Lane1 记忆注入；当前为空。
        bootstrap: Vec::new(),
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

    ActiveRun {
        run_id,
        cancel,
        started_at: std::time::Instant::now(),
    }
}

/// 简易 UUIDv7（避免为此引入额外依赖；server 已有 uuid）。
fn uuid_v7() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 近似 token 估算（字符/4，设计 §7 tokenizer 近似）。
fn estimate_tokens(s: &str) -> i64 {
    (s.chars().count() as i64 / 4).max(1)
}

/// 从库加载主会话历史（reset 之后），转成 oc-llm 消息，用于喂给模型。
///
/// 带 token 预算：从最近往前累计，超预算则截断（保留最近的）。
async fn load_history(store: &oc_store::Store, cfg: &SessionConfig) -> Vec<oc_llm::Message> {
    let entries = match store
        .writer()
        .load_transcript("main".into(), cfg.max_history_entries)
        .await
    {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "加载历史失败，按空历史处理");
            return Vec::new();
        }
    };

    // token 预算裁剪：从后往前累加，超预算丢弃更早的。
    let mut budget = cfg.history_token_budget;
    let mut kept: Vec<oc_llm::Message> = Vec::new();
    for e in entries.iter().rev() {
        let cost = e.tokens_est.max(1);
        if budget - cost < 0 && !kept.is_empty() {
            break;
        }
        budget -= cost;
        let role = match e.role {
            oc_store::Role::Assistant => oc_llm::MsgRole::Assistant,
            oc_store::Role::Tool => oc_llm::MsgRole::Tool,
            oc_store::Role::System => oc_llm::MsgRole::System,
            oc_store::Role::User => oc_llm::MsgRole::User,
        };
        kept.push(oc_llm::Message {
            role,
            content: e.content.clone(),
            tool_call_id: None,
        });
    }
    kept.reverse(); // 变回正序
    kept
}
