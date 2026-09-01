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
use oc_proto::{Event, RunId, SessionId};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::run::{self, RunCtx};

/// 会话配置（从 Config 派生）。
///
/// 不含 `session_id`：同一份 cfg 被 registry 复用来 spawn 多个会话 actor，
/// 会话 id 在 `spawn` 时单独传入。
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
    /// 加载的技能文档（~/.oc/skills/*.md），确定性排序后注入 prompt（设计 §4.4）。
    pub skills: Vec<oc_core::prompt::SkillBrief>,
    /// trigger 注入相关性阈值（Lane1，设计 §4.3）。
    pub trigger_threshold: f64,
    /// trigger 每轮最多注入条数。
    pub trigger_max_per_turn: usize,
    /// 模型上下文窗口（token），随 Usage 事件推给 client 显示。
    pub context_window: u32,
    /// standing intent 的 anti-nagging 参数（设计 §12.5）。
    pub intent_defaults: IntentDefaults,
    /// `~/.oc/soul/` 目录（SOUL/USER/MEMORY.md 所在）。
    ///
    /// `None` = 不落盘（测试/内存态）：dreaming 只做 DB 内 tier 提升，不重写 MEMORY.md。
    /// 设计 §13.1；server 本身不解析 OC_HOME，由 CLI 传入。
    pub soul_dir: Option<std::path::PathBuf>,
}

/// standing intent 的 anti-nagging 参数（设计 §12.5，源自 `ProactiveConfig`）。
///
/// 两类语义要分清：
/// - `cooldown_secs` / `budget` / `expiry_days`：**每条待办自己的**参数，落库在
///   `standing_intent` 行上、由 `allow_fire` 逐条判定。这里的值只是 `intent.add`
///   未显式指定时的**默认值**。
/// - `max_per_turn`：**每轮全局**注入上限，防一条消息命中多条待办时塞满上下文。
#[derive(Debug, Clone)]
pub struct IntentDefaults {
    pub cooldown_secs: i64,
    pub budget: u32,
    /// 多少天后过期；0 = 不过期。
    pub expiry_days: u32,
    pub max_per_turn: usize,
}

impl Default for IntentDefaults {
    /// 设计 §12.5 默认：cooldown 24h / budget 3 / 90 天过期 / ≤3 条每轮。
    fn default() -> Self {
        Self {
            cooldown_secs: 24 * 3600,
            budget: 3,
            expiry_days: 90,
            max_per_turn: 3,
        }
    }
}

/// 发给 session actor 的命令。
pub enum SessionCmd {
    /// 提交一轮用户输入，返回分配的 run_id。
    ///
    /// `sink`：本轮内联事件的出口（发起连接的出站队列，背压不丢；测试用广播）。
    Submit {
        text: String,
        sink: crate::sink::RunSink,
        reply: oneshot::Sender<RunId>,
    },
    /// 中止：hard=先 drain 排队轮再中止活跃（M4 完整）；M3 中止活跃 run。
    Abort { run_id: RunId, hard: bool },
    /// 手动压缩（/compact）：把当前历史摘要成 checkpoint。
    Compact,
    /// 活跃 run 结束通知（内部）。
    Finished { run_id: RunId, outcome: RunOutcome },
    /// 卡死诊断扫描（由心跳 tick 触发）：检查活跃 run 是否卡死。
    HealthScan,
}

/// actor 句柄。
#[derive(Clone)]
pub struct SessionHandle {
    tx: mpsc::Sender<SessionCmd>,
    /// 广播总线句柄：供 `broadcast_sink()` 构造沿用旧语义的 sink（测试/带外用）。
    events: broadcast::Sender<Event>,
}

impl SessionHandle {
    pub async fn submit(&self, text: String, sink: crate::sink::RunSink) -> Option<RunId> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(SessionCmd::Submit { text, sink, reply }).await.ok()?;
        rx.await.ok()
    }

    /// 构造一个广播 sink（run 事件走广播，沿用旧语义）。
    ///
    /// 生产路径用 `RunSink::Conn` 定向背压回连接；此助手主要给单测直接
    /// `subscribe()` 断言事件序列时用，或无连接归属的带外提交场景。
    pub fn broadcast_sink(&self) -> crate::sink::RunSink {
        crate::sink::RunSink::Broadcast(self.events.clone())
    }

    pub async fn abort(&self, run_id: RunId, hard: bool) {
        let _ = self.tx.send(SessionCmd::Abort { run_id, hard }).await;
    }

    /// 手动触发压缩（/compact）。
    pub async fn compact(&self) {
        let _ = self.tx.send(SessionCmd::Compact).await;
    }

    /// 触发一次卡死诊断扫描（心跳 tick 调用）。
    pub async fn health_scan(&self) {
        let _ = self.tx.send(SessionCmd::HealthScan).await;
    }
}

/// 启动 session actor，返回句柄。
///
/// `session_id`：本 actor 服务的会话 id（transcript 落库/加载、事件归属都用它）。
pub fn spawn(
    session_id: SessionId,
    cfg: SessionConfig,
    provider: Arc<dyn Provider>,
    events: broadcast::Sender<Event>,
    store: oc_store::Store,
    diag: crate::diag::SessionDiag,
) -> SessionHandle {
    let (tx, rx) = mpsc::channel(64);
    let handle = SessionHandle { tx: tx.clone(), events: events.clone() };
    tokio::spawn(actor_loop(session_id, cfg, provider, events, tx, rx, store, diag));
    handle
}

/// 活跃 run 的可中止句柄。
struct ActiveRun {
    run_id: RunId,
    cancel: CancellationToken,
    started_at: std::time::Instant,
}

#[allow(clippy::too_many_arguments)]
async fn actor_loop(
    session_id: SessionId,
    cfg: SessionConfig,
    provider: Arc<dyn Provider>,
    events: broadcast::Sender<Event>,
    self_tx: mpsc::Sender<SessionCmd>,
    mut rx: mpsc::Receiver<SessionCmd>,
    store: oc_store::Store,
    diag: crate::diag::SessionDiag,
) {
    let mut queue = RunQueue::new(cfg.queue_cap);
    let mut active: Option<ActiveRun> = None;
    // run_id → 本轮内联事件出口。submit 时存入，run 起步时取用，run 结束时清理。
    // 排队轮的 sink 也在此暂存，直到车道空出、该轮起步。
    let mut sinks: std::collections::HashMap<String, crate::sink::RunSink> =
        std::collections::HashMap::new();
    let sid = session_id.to_string();

    // 确保本会话存在。kind 统一记为 "main"（用户会话）；cron/dreaming 等隔离
    // 子会话不经本 actor，细分留待后续。
    if let Err(e) = store
        .writer()
        .ensure_session(sid.clone(), "main".into())
        .await
    {
        warn!(error = %e, session = %sid, "创建会话失败");
    }

    while let Some(cmd) = rx.recv().await {
        match cmd {
            SessionCmd::Submit { text, sink, reply } => {
                let run_id = RunId::new(uuid_v7());
                info!(session = %sid, run_id = %run_id, chars = text.chars().count(), "submit 受理");
                // 暂存本轮 sink（起步时取用）。
                sinks.insert(run_id.to_string(), sink);
                let _ = reply.send(run_id.clone());

                // 注意：**此处不落库用户消息**。落库推迟到该轮真正起步时（见 begin_run）。
                // 原因：同一 session 并发提交时，排队轮若在 submit 时就落库，会被前一个
                // 仍在跑的 run 后续追加的 assistant/tool 消息「插队」，导致历史顺序错乱
                // （B 的 user 消息被 A 的回复埋在中间，序列非法 → provider 400）。
                // 推迟到起步时落库，则用户消息落库顺序恒等于执行顺序。
                let turn = QueuedTurn {
                    run_id: run_id.to_string(),
                    text,
                };
                match queue.submit(turn) {
                    SubmitResult::Started(t) => {
                        info!(session = %sid, run_id = %run_id, "run 起步");
                        active = Some(
                            begin_run(
                                &session_id, &cfg, &provider, &events, &self_tx, &store, &diag,
                                t, &mut sinks,
                            )
                            .await,
                        );
                    }
                    SubmitResult::Queued => {
                        diag.set_queue_depth(queue.pending_len());
                        info!(session = %sid, run_id = %run_id, queued = queue.pending_len(), "轮已排队（车道忙）");
                    }
                    SubmitResult::Rejected => {
                        warn!(session = %sid, "队列已满，拒绝新轮");
                        diag.set_error("队列已满，拒绝新轮".to_string());
                        // 该轮不会跑，清理其暂存 sink，避免泄漏。
                        sinks.remove(run_id.as_str());
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
            SessionCmd::Compact => {
                // 手动 /compact：摘要当前历史（保留最近 keep_recent 条）。
                // 注意：compact 串行占用车道（与 OpenClaw 一致），期间新消息排队。
                let tc = std::time::Instant::now();
                info!(session = %sid, "compact 开始（占用车道）");
                diag.compact_start();
                compact_session(&store, &cfg, &provider, &events, &sid).await;
                diag.compact_done();
                info!(session = %sid, ms = tc.elapsed().as_millis(), "compact 结束");
            }
            SessionCmd::Finished { run_id, outcome } => {
                if active.as_ref().map(|a| &a.run_id) == Some(&run_id) {
                    let elapsed = active.as_ref().map(|a| a.started_at.elapsed().as_millis()).unwrap_or(0);
                    if !matches!(outcome, RunOutcome::Completed) {
                        warn!(session = %sid, run_id = %run_id, ?outcome, ms = elapsed, "run 非正常终态");
                        diag.set_error(format!("run {run_id} 终态: {outcome:?}"));
                    } else {
                        info!(session = %sid, run_id = %run_id, ms = elapsed, "run 完成");
                    }
                    diag.run_done(format!("{outcome:?}"));
                    active = None;
                    // 完成轮的 sink 已随 run 结束失效，清理。
                    sinks.remove(run_id.as_str());
                    // 取下一个排队轮。此刻前一轮已彻底完成（含其所有消息落库），
                    // 现在才落库并加载历史，保证顺序正确。
                    if let Some(next) = queue.complete_active() {
                        diag.set_queue_depth(queue.pending_len());
                        info!(session = %sid, run_id = %next.run_id, "起下一排队轮");
                        active = Some(
                            begin_run(
                                &session_id, &cfg, &provider, &events, &self_tx, &store, &diag,
                                next, &mut sinks,
                            )
                            .await,
                        );
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

/// 让一个（刚出队、即将占道的）轮真正起步：**此刻**才落库其用户消息、
/// 加载历史、Lane1 检索，然后 spawn run。
///
/// 关键不变式：用户消息的落库发生在「该轮拿到车道、前一轮已彻底完成」之后，
/// 因此落库顺序恒等于执行顺序——并发提交同一 session 时不会出现「排队轮的
/// 用户消息被前一轮后续追加的 assistant/tool 消息插队」的顺序错乱（否则历史
/// 序列非法，provider 400）。
#[allow(clippy::too_many_arguments)]
async fn begin_run(
    session_id: &SessionId,
    cfg: &SessionConfig,
    provider: &Arc<dyn Provider>,
    events: &broadcast::Sender<Event>,
    self_tx: &mpsc::Sender<SessionCmd>,
    store: &oc_store::Store,
    diag: &crate::diag::SessionDiag,
    turn: QueuedTurn,
    sinks: &mut std::collections::HashMap<String, crate::sink::RunSink>,
) -> ActiveRun {
    let sid = session_id.to_string();
    diag.run_start(turn.run_id.as_str());

    // 落库用户消息（重启不失忆）。失败仅告警，不阻断 run。
    let t0 = std::time::Instant::now();
    let est = estimate_tokens(&turn.text);
    if let Err(e) = store
        .writer()
        .append_entry(oc_store::NewEntry {
            session_id: sid.clone(),
            role: oc_store::Role::User,
            content: turn.text.clone(),
            tokens_est: est,
        })
        .await
    {
        warn!(error = %e, "落库用户消息失败");
        diag.set_error(format!("落库用户消息失败: {e}"));
    }
    debug!(session = %sid, run_id = %turn.run_id, ms = t0.elapsed().as_millis(), "落库用户消息完成");

    // 显式"记住…"写入路径（设计 §4.1）：用户显式指令 → curated + Owner + 审计。
    persist_explicit_memory(store, &turn.text).await;

    // 加载历史（含刚落库的本轮用户消息）+ Lane1 记忆检索。
    let th = std::time::Instant::now();
    let history = load_history(store, cfg, &sid).await;
    debug!(
        session = %sid, run_id = %turn.run_id,
        entries = history.len(), ms = th.elapsed().as_millis(),
        "历史加载完成"
    );
    let mut boot = lane1_bootstrap(store, cfg, &turn.text).await;
    // standing intent 触发（设计 §12.3）：命中话题的待办注入隐藏上下文提醒模型。
    // 与 Lane1 记忆共用 bootstrap 注入管线；失败绝不阻塞回复。
    boot.extend(intent_scan(store, cfg, &turn.text).await);
    let sink = sinks
        .remove(&turn.run_id)
        .unwrap_or_else(|| default_sink(events));

    start_run(session_id, cfg, provider, events, self_tx, store, diag, turn, history, boot, sink)
}

/// 启动一个 run 任务（含 panic 隔离），返回可中止句柄。
#[allow(clippy::too_many_arguments)]
fn start_run(
    session_id: &SessionId,
    cfg: &SessionConfig,
    provider: &Arc<dyn Provider>,
    events: &broadcast::Sender<Event>,
    self_tx: &mpsc::Sender<SessionCmd>,
    store: &oc_store::Store,
    diag: &crate::diag::SessionDiag,
    turn: QueuedTurn,
    history: Vec<oc_llm::Message>,
    bootstrap: Vec<oc_core::prompt::MemLine>,
    sink: crate::sink::RunSink,
) -> ActiveRun {
    let cancel = CancellationToken::new();
    let run_id = RunId::new(turn.run_id.clone());
    let ctx = RunCtx {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
        user_text: turn.text,
        system_prompt: cfg.system_prompt.clone(),
        model: cfg.model.clone(),
        provider: Arc::clone(provider),
        sink,
        events: events.clone(),
        cancel: cancel.clone(),
        idle_timeout: cfg.idle_timeout,
        run_timeout: cfg.run_timeout,
        tools: cfg.tools.clone(),
        store: store.clone(),
        history,
        soul: cfg.soul.clone(),
        skills: cfg.skills.clone(),
        // Lane1 记忆注入（curated，trigger 预筛命中的）。
        bootstrap,
        compact_cfg: oc_core::compaction::CompactCfg {
            budget: cfg.history_token_budget,
            ..Default::default()
        },
        context_window: cfg.context_window,
        diag: diag.clone(),
    };

    let self_tx = self_tx.clone();
    let rid = run_id.clone();
    let span = tracing::info_span!("run", run_id = %run_id, session = %session_id);
    tokio::spawn(async move {
        use tracing::Instrument;
        // panic 隔离：单 run panic 不拖垮进程（设计 §10.3）。
        let fut = std::panic::AssertUnwindSafe(run::drive(ctx).instrument(span));
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

/// 兜底 sink：正常路径下每轮都有暂存 sink，此处仅防御性回退到广播，
/// 保证即便映射意外缺失，事件也不至于凭空丢弃（会走旧广播语义）。
fn default_sink(events: &broadcast::Sender<Event>) -> crate::sink::RunSink {
    crate::sink::RunSink::Broadcast(events.clone())
}

/// 近似 token 估算（字符/4，设计 §7 tokenizer 近似）。
fn estimate_tokens(s: &str) -> i64 {
    (s.chars().count() as i64 / 4).max(1)
}

/// 从库加载主会话历史（reset 之后），转成 oc-llm 消息，用于喂给模型。
///
/// 带 token 预算：从最近往前累计，超预算则截断（保留最近的）。
async fn load_history(store: &oc_store::Store, cfg: &SessionConfig, session_id: &str) -> Vec<oc_llm::Message> {
    let entries = match store
        .writer()
        .load_transcript(session_id.into(), cfg.max_history_entries)
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
        // 历史重放降级：entry 表只存了扁平 role+content，没有工具调用的关联
        // id。原生 `tool` 消息要求前面有带匹配 tool_calls 的 assistant，且自身
        // 需 tool_call_id——这些库里都没有。因此把历史里的工具结果降级为普通
        // 文本消息，保证重放序列对 provider 合法。实时那一轮的原生工具调用不走
        // 这里（见 run.rs），不受影响。
        let (role, content) = match e.role {
            oc_store::Role::Assistant => (oc_llm::MsgRole::Assistant, e.content.clone()),
            oc_store::Role::System => (oc_llm::MsgRole::System, e.content.clone()),
            oc_store::Role::User => (oc_llm::MsgRole::User, e.content.clone()),
            // 工具结果降级为 user 文本，避免产出缺 tool_call_id 的裸 tool 消息。
            oc_store::Role::Tool => (
                oc_llm::MsgRole::User,
                format!("【历史工具结果】\n{}", e.content),
            ),
        };
        kept.push(oc_llm::Message {
            role,
            content,
            tool_call_id: None,
            tool_calls: vec![],
            // 历史重放不带 reasoning（thinking 内容不落库，仅实时轮回喂）。
            reasoning: None,
        });
    }
    kept.reverse(); // 变回正序
    kept
}

/// 手动/自动摘要压缩：把 reset 之后、除最近 keep_recent 条以外的历史总结成一条
/// checkpoint，落库（推进 reset_at + 插摘要 entry）。失败仅告警，不影响后续。
///
/// keep_recent 取压缩配置的近期保留条数（沿用 CompactCfg 默认语义）。
async fn compact_session(
    store: &oc_store::Store,
    cfg: &SessionConfig,
    provider: &Arc<dyn Provider>,
    events: &broadcast::Sender<Event>,
    session_id: &str,
) {
    let keep_recent = oc_core::compaction::CompactCfg::default().keep_recent;

    let entries = match store
        .writer()
        .load_transcript(session_id.into(), cfg.max_history_entries)
        .await
    {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "compact：加载历史失败");
            notify_compact(events, session_id, "压缩失败：加载历史出错");
            return;
        }
    };
    // 少于阈值不值得压缩：给出明确反馈，而非静默跳过。
    if entries.len() <= keep_recent + 1 {
        notify_compact(
            events,
            session_id,
            &format!("当前上下文较短（{} 条），无需压缩", entries.len()),
        );
        return;
    }

    // 切点：保留最近 keep_recent 条，其余进摘要区。
    let cut = entries.len() - keep_recent;
    let older = &entries[..cut];
    let up_to_seq = older.last().map(|e| e.seq).unwrap_or(0);

    // 映射为 llm 消息喂给摘要模型。
    let msgs: Vec<oc_llm::Message> = older
        .iter()
        .map(|e| oc_llm::Message {
            role: match e.role {
                oc_store::Role::Assistant => oc_llm::MsgRole::Assistant,
                oc_store::Role::System => oc_llm::MsgRole::System,
                oc_store::Role::Tool => oc_llm::MsgRole::User,
                oc_store::Role::User => oc_llm::MsgRole::User,
            },
            content: e.content.clone(),
            tool_call_id: None,
            tool_calls: vec![],
            reasoning: None,
        })
        .collect();

    let older_count = older.len();
    let input_chars: usize = msgs.iter().map(|m| m.content.chars().count()).sum();
    info!(session = %session_id, msgs = older_count, input_chars, "compact：调摘要模型");
    let Some(summary) = crate::summarize::summarize(provider, &cfg.model, &msgs).await else {
        warn!(session = %session_id, msgs = older_count, "compact：摘要为空/失败，跳过");
        notify_compact(events, session_id, "压缩失败：摘要生成为空或超时");
        return;
    };

    if let Err(e) = store
        .writer()
        .compact_with_summary(session_id.into(), up_to_seq, summary)
        .await
    {
        warn!(error = %e, "compact：落库失败");
        notify_compact(events, session_id, "压缩失败：写入出错");
        return;
    }

    notify_compact(
        events,
        session_id,
        &format!("已压缩上下文：{older_count} 条历史消息总结为摘要"),
    );
}

/// 向 client 推一条 compact 相关的系统通知（走 Proactive 通道，归属本会话）。
fn notify_compact(events: &broadcast::Sender<Event>, session_id: &str, text: &str) {
    let _ = events.send(Event::Proactive {
        session: oc_proto::SessionId::new(session_id.to_string()),
        kind: oc_proto::ProactiveKind::Wake,
        text: text.to_string(),
        source: oc_proto::ProactiveSource::Heartbeat,
    });
}

/// 把用户消息分词（词法检索用）。
///
/// 中文无空格，按空白/标点切后往往整句成一个词，contains 命中率低。
/// 折中方案：空白/标点切分的词 + 对每段连续字符生成 2-gram（相邻两字），
/// 让"回复"这类双字词能被 contains 命中。去重后返回。
fn tokenize(msg: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for seg in msg.split(|c: char| c.is_whitespace() || c.is_ascii_punctuation()) {
        let chars: Vec<char> = seg.chars().collect();
        if chars.len() < 2 {
            continue;
        }
        // 整段作为词（利于英文单词、短中文短语）。
        terms.push(seg.to_string());
        // 2-gram：相邻两字，覆盖中文双字词。
        if chars.len() > 2 {
            for w in chars.windows(2) {
                terms.push(w.iter().collect());
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms
}

/// 显式记忆写入（设计 §4.1 + §4.5(e)）：识别"记住…"指令 → 写 curated 记忆 + 审计。
///
/// origin 走 core::classify_origin(UserExplicit) = Owner（唯一产生 Owner 的路径）。
/// id/hash 用内容派生，天然去重（同内容多次"记住"upsert 同一行）。
///
/// **两条路径**（P1-3）：
/// - 内容能归到偏好主题（`extract_pref_key` 命中）→ 走 [`supersede`] 判定，
///   同主题的新值**就地替换**旧值（"我改用 Neovim" 顶掉 "我用 VS Code"）。
/// - 归不到主题 → 走原路径（upsert，靠内容哈希去重），行为不变。
///
/// **失败仅告警，不阻塞回复**。
async fn persist_explicit_memory(store: &oc_store::Store, user_msg: &str) {
    use oc_core::memory::{
        classify_origin, detect_explicit_memory, extract_pref_key, supersede, Pref, SupersedePlan,
        WriteSource,
    };

    let Some(explicit) = detect_explicit_memory(user_msg) else {
        return;
    };
    let origin = classify_origin(WriteSource::UserExplicit); // = Owner
    let hash = content_hash(&explicit.content);
    let id = format!("mem-{hash}");

    // 偏好主题判定（纯函数，保守：只认明确主题，未命中则 None）。
    let pref_key = extract_pref_key(&explicit.content);

    // 命中主题 → 查同主题既有偏好，让 core 判 replace/add/ignore。
    // 查询失败不放弃写入：退化成 Add（宁可多留一条，也不因读失败丢掉用户的话）。
    let mut to_delete: Option<String> = None;
    if let Some(key) = &pref_key {
        match store.writer().memory_by_pref_key(key.clone()).await {
            Ok(rows) => {
                let existing: Vec<Pref> = rows
                    .iter()
                    .map(|r| Pref {
                        id: r.id.clone(),
                        key: key.clone(),
                        value: r.text.clone(),
                    })
                    .collect();
                let incoming = Pref {
                    id: id.clone(),
                    key: key.clone(),
                    value: explicit.content.clone(),
                };
                match supersede(&existing, &incoming) {
                    SupersedePlan::Ignore => {
                        debug!(key = %key, "偏好未变化，跳过写入");
                        return;
                    }
                    SupersedePlan::Add => {}
                    SupersedePlan::Replace { existing_id } => {
                        // 同主题旧值待清理。注意**先写新、后删旧**（见下方顺序说明）。
                        to_delete = Some(existing_id);
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, key = %key, "偏好查询失败，退化为直接写入");
            }
        }
    }

    let mem = oc_store::NewMemory {
        id: id.clone(),
        tier: oc_store::Tier::Curated,
        origin: match origin {
            oc_core::memory::Origin::Owner => oc_store::Origin::Owner,
            oc_core::memory::Origin::Agent => oc_store::Origin::Agent,
            oc_core::memory::Origin::Untrusted => oc_store::Origin::Untrusted,
            oc_core::memory::Origin::System => oc_store::Origin::System,
        },
        text: explicit.content.clone(),
        keywords: None,
        importance: 0.8, // 用户显式指定 → 高重要度。
        content_hash: hash,
        pref_key: pref_key.clone(),
    };

    if let Err(e) = store.writer().upsert_memory(mem).await {
        warn!(error = %e, "显式记忆写入失败");
        return;
    }

    // 删旧偏好（supersede Replace）。**顺序刻意是先写新、后删旧**：
    // 反过来若中途失败会两头空（旧的删了、新的没写进去，用户的偏好凭空消失）。
    // 当前顺序最坏情况是新旧短暂并存，下一轮 supersede 会收敛，不丢数据。
    //
    // 边界：新旧内容哈希相同时 id 也相同，upsert 已就地更新同一行，此时不能删
    // （会把刚写的删掉）。同值本应被 Ignore 拦住，这里再挡一层。
    if let Some(old_id) = to_delete {
        if old_id == id {
            debug!(id = %id, "新旧同 id，upsert 已覆盖，跳过删除");
        } else {
            match store.writer().delete_memory(old_id.clone()).await {
                Ok(_) => {
                    info!(old = %old_id, new = %id, key = ?pref_key, "偏好就地替换");
                    if let Err(e) = store
                        .writer()
                        .write_audit("owner".into(), "supersede".into(), Some(old_id))
                        .await
                    {
                        warn!(error = %e, "偏好替换审计失败");
                    }
                }
                Err(e) => warn!(error = %e, old = %old_id, "旧偏好删除失败（新值已写入）"),
            }
        }
    }

    if let Err(e) = store
        .writer()
        .write_audit("owner".into(), "remember".into(), Some(id))
        .await
    {
        warn!(error = %e, "记忆写入审计失败");
    }
}

/// 内容派生哈希（FNV-1a，16 位十六进制），用于记忆去重 id 与 content_hash。
fn content_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Lane1 记忆检索（设计 §4.3）：取候选 → core trigger 预筛 → 命中的 curated
/// 记忆作为 bootstrap 注入。**失败绝不阻塞回复**（返回空）。
async fn lane1_bootstrap(
    store: &oc_store::Store,
    cfg: &SessionConfig,
    user_msg: &str,
) -> Vec<oc_core::prompt::MemLine> {
    use oc_core::memory::{trigger_prefilter, MemCandidate, Origin as CoreOrigin, Tier as CoreTier};

    let terms = tokenize(user_msg);
    if terms.is_empty() {
        return Vec::new();
    }

    // 仅取 curated 候选（自动注入只限 curated）。
    let rows = match store
        .writer()
        .search_candidates(terms.clone(), Some(oc_store::Tier::Curated), 32)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Lane1 检索失败，跳过记忆注入");
            return Vec::new();
        }
    };

    // 映射为 core 候选。
    let cands: Vec<MemCandidate> = rows
        .iter()
        .map(|r| MemCandidate {
            id: r.id.clone(),
            tier: CoreTier::Curated,
            origin: match r.origin {
                oc_store::Origin::Owner => CoreOrigin::Owner,
                oc_store::Origin::Agent => CoreOrigin::Agent,
                oc_store::Origin::Untrusted => CoreOrigin::Untrusted,
                oc_store::Origin::System => CoreOrigin::System,
            },
            text: r.text.clone(),
            importance: r.importance,
            last_used_secs: r.last_used_at.unwrap_or(r.created_at) / 1000,
        })
        .collect();

    let hits = trigger_prefilter(
        user_msg,
        &cands,
        &terms,
        cfg.trigger_threshold,
        cfg.trigger_max_per_turn,
    );

    // 命中 id → 取全文作为 MemLine 注入。
    hits.iter()
        .filter_map(|id| cands.iter().find(|c| &c.id == id))
        .map(|c| oc_core::prompt::MemLine {
            key: c.id.clone(),
            text: c.text.clone(),
        })
        .collect()
}

/// standing intent 触发扫描（设计 §4.5(f) / §12.3）。
///
/// 「话题触发式待办」：入站消息词法命中某条待办的关键词 → 套 anti-nagging
/// （cooldown/budget/expiry，逐条用该行自己的参数）→ 允许则注入隐藏上下文提醒
/// 模型、并抬 fired_count；拒绝则**静默跳过**（不打扰用户）。
///
/// 与 cron（到点触发）互补：cron 走隔离子会话（[crate::proactive]），本函数只在
/// 主会话入站路径触发——天然避免 cron/dreaming 子会话误注入。
///
/// **失败绝不阻塞回复**：任何一步出错返回空、仅告警（同 [`lane1_bootstrap`] 规格）。
async fn intent_scan(
    store: &oc_store::Store,
    cfg: &SessionConfig,
    user_msg: &str,
) -> Vec<oc_core::prompt::MemLine> {
    use oc_core::memory::{intent_prefilter, StandingIntent};
    use oc_core::proactive::{allow_fire, FireDecision, IntentState, NagCfg};

    let rows = match store.writer().intent_list().await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "standing intent：读取失败，跳过本轮触发");
            return Vec::new();
        }
    };
    if rows.is_empty() {
        return Vec::new();
    }

    // 词法预筛（纯函数）：命中任一关键词即候选。
    let intents: Vec<StandingIntent> = rows
        .iter()
        .map(|r| StandingIntent { id: r.id.clone(), keywords: r.keywords.clone() })
        .collect();
    let hit_ids = intent_prefilter(user_msg, &intents);
    if hit_ids.is_empty() {
        return Vec::new();
    }

    let now = now_secs();
    let mut out = Vec::new();
    for id in hit_ids {
        // 每轮注入条数上限（anti-nagging，设计 §12.5 ≤3）。
        let cap = cfg.intent_defaults.max_per_turn;
        if out.len() >= cap {
            debug!(cap, "standing intent：达每轮注入上限，其余顺延");
            break;
        }
        let Some(row) = rows.iter().find(|r| r.id == id) else {
            continue;
        };

        // anti-nagging 判定用**该行自己的**参数（各条可不同）。
        //
        // expiry 语义换算：store 存**绝对时间点** expiry_at，core 收**距创建的时长**
        // expiry_secs。注意 `NagCfg::expiry_secs == 0` 表示「永不过期」，所以
        // expiry_at ≤ created_at（创建即过期 / 已过期）**不能**夹成 0——那会把
        // 「已过期」翻转成「永不过期」。这种退化情形直接判过期跳过。
        // expiry_at > created_at 的正常情形交给 allow_fire 判（now-created > 时长）。
        let expiry_secs = match row.expiry_at {
            None => 0, // 不过期
            Some(at) => {
                let span = at - row.created_at;
                if span <= 0 {
                    debug!(intent = %row.id, "standing intent：已过期，跳过");
                    continue;
                }
                span
            }
        };
        let state = IntentState {
            created_secs: row.created_at,
            last_fired_secs: row.last_fired_at,
            fired_count: row.fired_count,
        };
        let nag = NagCfg {
            cooldown_secs: row.cooldown_secs,
            budget: row.budget,
            expiry_secs,
        };

        match allow_fire(&state, now, &nag) {
            FireDecision::Allow => {}
            FireDecision::Deny(reason) => {
                // 静默跳过：这正是 anti-nagging 的目的，不向用户暴露。
                debug!(intent = %row.id, ?reason, "standing intent：命中但拒绝触发");
                continue;
            }
        }

        // 注入隐藏上下文：标注「待办提醒」以与记忆区分，让模型自然提起。
        out.push(oc_core::prompt::MemLine {
            key: format!("intent-{}", row.id),
            text: format!("待办提醒（用户此前交代，现因话题命中而唤起）：{}", row.text),
        });
        info!(intent = %row.id, fired = row.fired_count + 1, "standing intent：触发注入");

        // 记账：抬 fired_count + 记 last_fired_at。失败仅告警——宁可多提醒一次，
        // 也不因记账失败而丢掉这次提醒（已注入的不回滚）。
        if let Err(e) = store.writer().intent_mark_fired(row.id.clone(), now).await {
            warn!(intent = %row.id, error = %e, "standing intent：触发记账失败");
        }
        // 审计（与 cron 触发同规格）。
        if let Err(e) = store
            .writer()
            .write_audit("intent".into(), "fire".into(), Some(row.id.clone()))
            .await
        {
            warn!(intent = %row.id, error = %e, "standing intent：审计失败");
        }
    }
    out
}

/// 当前 unix 秒（standing intent / anti-nagging 的时间语义）。
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
