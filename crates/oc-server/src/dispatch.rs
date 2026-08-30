//! 请求分发（设计 §7.2）。
//!
//! M2：处理 req → 返回 res，必要时广播 event。无 agent loop，`chat.send`
//! 以 **echo** 演示完整事件流（lifecycle → assistant delta → lifecycle end）。

use std::sync::Arc;

use oc_proto::{
    ChatAbortParams, ChatSendParams, ConnectParams, Features, Frame, Method, MethodOk, ProtoError,
    Req, ResResult, SessionId, SessionResetParams, Snapshot, PROTO_VERSION,
};
use tokio::sync::mpsc;

use crate::sink::RunSink;
use crate::state::{CachedRes, ServerState};

/// 处理一个请求，返回应答载荷。副作用（事件广播）在此内部完成。
///
/// `out_tx`：本连接的出站帧队列——`chat.send` 用它构造 per-run sink，
/// 让本轮内联事件（文本/工具/审批）背压式定向回发到这条连接（P0-1）。
pub async fn handle_req(req: &Req, state: &Arc<ServerState>, out_tx: &mpsc::Sender<Frame>) -> ResResult {
    // 幂等：side-effecting 方法命中缓存直接返回首个结果。
    if let Some(key) = &req.idempotency_key {
        if let Some(cached) = state.idem_get(key) {
            return ResResult::Ok(cached.ok);
        }
    }

    let result = match &req.method {
        Method::Connect(p) => handle_connect(p, state),
        Method::ChatSend(p) => handle_chat_send(p, state, out_tx).await,
        Method::ChatAbort(p) => handle_chat_abort(p, state).await,
        Method::ApprovalReply(p) => {
            state.resolve_approval(&p.approval_id, p.allow);
            Ok(MethodOk::Empty)
        }
        Method::SessionReset(p) => handle_session_reset(p, state).await,
        Method::Compact(p) => handle_compact(p, state).await,
        Method::SessionsList => handle_sessions_list(state).await,
        Method::Diagnostics => handle_diagnostics(state).await,
        Method::Status => Ok(MethodOk::Status(snapshot(state, &SessionId::main()))),
        Method::Health => Ok(MethodOk::Health(oc_proto::HealthOk {
            ok: true,
            db_version: oc_store::migrate::TARGET_VERSION,
        })),
        Method::ChatHistory(p) => {
            let limit = p.limit.unwrap_or(200) as i64;
            let session = p.session.clone().unwrap_or_else(SessionId::main);
            match state.store().writer().load_transcript(session.to_string(), limit).await {
                Ok(entries) => {
                    let out = entries
                        .into_iter()
                        .map(|e| oc_proto::Entry {
                            seq: e.seq,
                            role: match e.role {
                                oc_store::Role::Assistant => oc_proto::Role::Assistant,
                                oc_store::Role::Tool => oc_proto::Role::Tool,
                                oc_store::Role::System => oc_proto::Role::System,
                                oc_store::Role::User => oc_proto::Role::User,
                            },
                            content: e.content,
                            created_at: e.created_at,
                        })
                        .collect();
                    Ok(MethodOk::History(out))
                }
                Err(e) => Err(ProtoError {
                    kind: oc_proto::ErrorKind::Internal,
                    message: format!("加载历史失败: {e}"),
                }),
            }
        }
        Method::TasksList => Ok(MethodOk::Tasks(state.ledger().list())),
        Method::TasksCancel(p) => {
            state.ledger().cancel(&p.task_id);
            Ok(MethodOk::Empty)
        }
        Method::CronAdd(p) => handle_cron_add(p, state).await,
        Method::CronList => handle_cron_list(state).await,
        Method::CronRm(p) => handle_cron_rm(p, state).await,
        Method::MemorySearch(p) => handle_memory_search(p, state).await,
    };

    match result {
        Ok(ok) => {
            // 写幂等缓存。
            if let Some(key) = &req.idempotency_key {
                state.idem_put(key.clone(), CachedRes { ok: ok.clone() });
            }
            ResResult::Ok(ok)
        }
        Err(e) => ResResult::Err(e),
    }
}

fn handle_connect(p: &ConnectParams, state: &Arc<ServerState>) -> Result<MethodOk, ProtoError> {
    if p.proto_version != PROTO_VERSION {
        return Err(ProtoError {
            kind: oc_proto::ErrorKind::ProtoVersionMismatch,
            message: format!(
                "协议版本不匹配：client={}, server={PROTO_VERSION}",
                p.proto_version
            ),
        });
    }
    Ok(MethodOk::Hello {
        features: Features {
            ws_remote: false, // WS 远程为 Phase 2 feature
            memory_vec: false,
            sandbox: false,
            proto_version: PROTO_VERSION,
        },
        snapshot: snapshot(state, &SessionId::main()),
    })
}

/// M3：提交到主会话车道，返回分配的 run_id；实际处理经 per-run sink 定向推送。
async fn handle_chat_send(
    p: &ChatSendParams,
    state: &Arc<ServerState>,
    out_tx: &mpsc::Sender<Frame>,
) -> Result<MethodOk, ProtoError> {
    // 缺省路由到 main；未知 id 由 registry 懒创建（隐式建会话）。
    let session = p.session.clone().unwrap_or_else(SessionId::main);
    let handle = state.registry().get_or_spawn(&session);
    // 本轮内联事件定向回发到这条连接（背压不丢，P0-1）。
    let sink = RunSink::Conn(out_tx.clone());
    match handle.submit(p.text.clone(), sink).await {
        Some(run_id) => Ok(MethodOk::ChatSend { run_id }),
        None => Err(ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: "会话车道不可用".to_string(),
        }),
    }
}

/// M3：中止活跃 run（hard 语义在 M4 完整）。
///
/// abort 不带 session 字段，故对所有活跃会话广播中止请求；各 actor 只中止
/// 匹配 run_id 的活跃 run（run_id 全局唯一），互不影响。
async fn handle_chat_abort(
    p: &ChatAbortParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    state.registry().abort_all(p.run_id.clone(), p.hard).await;
    Ok(MethodOk::Empty)
}

/// 重置指定会话（缺省 main）：推进上下文起点，transcript 保留。
async fn handle_session_reset(
    p: &SessionResetParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    let session = p.session.clone().unwrap_or_else(SessionId::main);
    state
        .store()
        .writer()
        .reset_session(session.to_string())
        .await
        .map_err(|e| ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: format!("重置会话失败: {e}"),
        })?;
    Ok(MethodOk::Empty)
}

/// 手动压缩指定会话（/compact，缺省 main）：触发摘要，立即返回（异步执行）。
async fn handle_compact(
    p: &oc_proto::CompactParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    let session = p.session.clone().unwrap_or_else(SessionId::main);
    let handle = state.registry().get_or_spawn(&session);
    handle.compact().await;
    Ok(MethodOk::Empty)
}

/// 列出所有会话（sessions.list）。
async fn handle_sessions_list(state: &Arc<ServerState>) -> Result<MethodOk, ProtoError> {
    let rows = state.store().writer().session_list().await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("列出会话失败: {e}"),
    })?;
    let out = rows
        .into_iter()
        .map(|s| oc_proto::SessionView {
            id: SessionId::new(s.id),
            kind: s.kind,
            created_at: s.created_at,
            reset_at: s.reset_at,
        })
        .collect();
    Ok(MethodOk::Sessions(out))
}

/// 整机诊断快照（`oc debug`）：会话运行时状态 + 写线程健康 + 订阅者数。
async fn handle_diagnostics(state: &Arc<ServerState>) -> Result<MethodOk, ProtoError> {
    // 写线程健康：发一个廉价读命令（session_list），能回即视为存活。
    let store_writer_alive = state.store().writer().session_list().await.is_ok();
    let snap = oc_proto::DiagnosticsSnapshot {
        uptime_secs: state.diag().uptime_secs(),
        sessions: state.diag().snapshot_sessions(),
        store_writer_alive,
        event_subscribers: state.subscriber_count(),
        sampled_at: now_millis(),
        proto_version: PROTO_VERSION,
    };
    Ok(MethodOk::Diagnostics(snap))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 新增 cron：校验表达式（core::next_fire）→ 算首次触发 → 落库。
async fn handle_cron_add(
    p: &oc_proto::CronAddParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    let now = now_secs();
    // 校验 + 算首次触发。
    let next_at = match oc_core::proactive::next_fire(&p.expr, now) {
        Ok(n) => n,
        Err(e) => {
            return Err(ProtoError {
                kind: oc_proto::ErrorKind::BadRequest,
                message: format!("cron 表达式非法: {e}"),
            })
        }
    };
    let id = format!("cron-{}", uuid::Uuid::now_v7());
    let cron = oc_store::NewCron {
        id: id.clone(),
        expr: p.expr.clone(),
        prompt: p.prompt.clone(),
        tz: p.tz.clone(),
        next_at,
    };
    state.store().writer().cron_add(cron).await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("新增 cron 失败: {e}"),
    })?;
    Ok(MethodOk::CronAdd { cron_id: oc_proto::CronId::new(id) })
}

async fn handle_cron_list(state: &Arc<ServerState>) -> Result<MethodOk, ProtoError> {
    let rows = state.store().writer().cron_list().await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("列出 cron 失败: {e}"),
    })?;
    let out = rows
        .into_iter()
        .map(|c| oc_proto::CronSpec {
            id: oc_proto::CronId::new(c.id),
            expr: c.expr,
            prompt: c.prompt,
            tz: c.tz,
            enabled: c.enabled,
            next_at: c.next_at,
        })
        .collect();
    Ok(MethodOk::CronList(out))
}

async fn handle_cron_rm(
    p: &oc_proto::CronRmParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    state
        .store()
        .writer()
        .cron_rm(p.cron_id.to_string())
        .await
        .map_err(|e| ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: format!("删除 cron 失败: {e}"),
        })?;
    Ok(MethodOk::Empty)
}

/// 记忆检索（调试/自省）：Lane1 词法排名（复用 core::rank）。
async fn handle_memory_search(
    p: &oc_proto::MemSearchParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    use oc_core::memory::{rank, MemCandidate, Origin as CoreOrigin, Tier as CoreTier, RankCfg};

    let terms = tokenize(&p.query);
    let limit = p.limit.unwrap_or(10) as i64;
    let rows = state
        .store()
        .writer()
        .search_candidates(terms.clone(), None, 64)
        .await
        .map_err(|e| ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: format!("记忆检索失败: {e}"),
        })?;

    let cands: Vec<MemCandidate> = rows
        .iter()
        .map(|r| MemCandidate {
            id: r.id.clone(),
            tier: match r.tier {
                oc_store::Tier::Curated => CoreTier::Curated,
                oc_store::Tier::Episodic => CoreTier::Episodic,
                oc_store::Tier::Prospective => CoreTier::Prospective,
                oc_store::Tier::Review => CoreTier::Review,
            },
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

    let ranked = rank(&cands, &terms, now_secs(), &RankCfg::default());
    let hits: Vec<oc_proto::MemHit> = ranked
        .into_iter()
        .filter(|r| r.score > 0.0)
        .take(limit as usize)
        .filter_map(|r| {
            let c = cands.iter().find(|c| c.id == r.id)?;
            Some(oc_proto::MemHit {
                id: oc_proto::MemoryId::new(r.id.clone()),
                tier: match c.tier {
                    CoreTier::Curated => "curated",
                    CoreTier::Episodic => "episodic",
                    CoreTier::Prospective => "prospective",
                    CoreTier::Review => "review",
                }
                .to_string(),
                text: c.text.clone(),
                score: r.score as f32,
            })
        })
        .collect();
    Ok(MethodOk::MemorySearch(hits))
}

/// 词法分词（与 session::tokenize 同策略：空白/标点切 + CJK 2-gram）。
fn tokenize(msg: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for seg in msg.split(|c: char| c.is_whitespace() || c.is_ascii_punctuation()) {
        let chars: Vec<char> = seg.chars().collect();
        if chars.len() < 2 {
            continue;
        }
        terms.push(seg.to_string());
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn snapshot(state: &Arc<ServerState>, session: &SessionId) -> Snapshot {
    Snapshot {
        active_run: None,
        queued_turns: 0,
        background_tasks: 0,
        session: session.clone(),
        context_window: state.context_window(),
        last_input_tokens: state.last_input_tokens(session),
    }
}
