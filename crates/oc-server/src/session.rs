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
    /// 加载的技能文档（~/.oc/skills/*.md），确定性排序后注入 prompt（设计 §4.4）。
    pub skills: Vec<oc_core::prompt::SkillBrief>,
    /// trigger 注入相关性阈值（Lane1，设计 §4.3）。
    pub trigger_threshold: f64,
    /// trigger 每轮最多注入条数。
    pub trigger_max_per_turn: usize,
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

                // 显式"记住…"写入路径（设计 §4.1）：用户显式指令 → curated + Owner + 审计。
                persist_explicit_memory(&store, &text).await;

                let turn = QueuedTurn {
                    run_id: run_id.to_string(),
                    text,
                };
                match queue.submit(turn) {
                    SubmitResult::Started(t) => {
                        let history = load_history(&store, &cfg).await;
                        let boot = lane1_bootstrap(&store, &cfg, &t.text).await;
                        active = Some(start_run(&cfg, &provider, &events, &self_tx, &store, t, history, boot));
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
                        let boot = lane1_bootstrap(&store, &cfg, &next.text).await;
                        active = Some(start_run(&cfg, &provider, &events, &self_tx, &store, next, history, boot));
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
    bootstrap: Vec<oc_core::prompt::MemLine>,
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
        skills: cfg.skills.clone(),
        // Lane1 记忆注入（curated，trigger 预筛命中的）。
        bootstrap,
        compact_cfg: oc_core::compaction::CompactCfg {
            budget: cfg.history_token_budget,
            ..Default::default()
        },
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
        });
    }
    kept.reverse(); // 变回正序
    kept
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

/// 显式记忆写入（设计 §4.1）：识别"记住…"指令 → 写 curated 记忆 + 审计。
///
/// origin 走 core::classify_origin(UserExplicit) = Owner（唯一产生 Owner 的路径）。
/// id/hash 用内容派生，天然去重（同内容多次"记住"upsert 同一行）。
/// **失败仅告警，不阻塞回复**。
async fn persist_explicit_memory(store: &oc_store::Store, user_msg: &str) {
    use oc_core::memory::{classify_origin, detect_explicit_memory, WriteSource};

    let Some(explicit) = detect_explicit_memory(user_msg) else {
        return;
    };
    let origin = classify_origin(WriteSource::UserExplicit); // = Owner
    let hash = content_hash(&explicit.content);
    let id = format!("mem-{hash}");

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
    };

    if let Err(e) = store.writer().upsert_memory(mem).await {
        warn!(error = %e, "显式记忆写入失败");
        return;
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
