//! 请求分发（设计 §7.2）。
//!
//! M2：处理 req → 返回 res，必要时广播 event。无 agent loop，`chat.send`
//! 以 **echo** 演示完整事件流（lifecycle → assistant delta → lifecycle end）。

use std::sync::Arc;

use oc_proto::{
    ChatAbortParams, ChatSendParams, ConnectParams, Features, Method, MethodOk, ProtoError, Req,
    ResResult, SessionId, Snapshot, PROTO_VERSION,
};

use crate::state::{CachedRes, ServerState};

/// 处理一个请求，返回应答载荷。副作用（事件广播）在此内部完成。
pub async fn handle_req(req: &Req, state: &Arc<ServerState>) -> ResResult {
    // 幂等：side-effecting 方法命中缓存直接返回首个结果。
    if let Some(key) = &req.idempotency_key {
        if let Some(cached) = state.idem_get(key) {
            return ResResult::Ok(cached.ok);
        }
    }

    let result = match &req.method {
        Method::Connect(p) => handle_connect(p),
        Method::ChatSend(p) => handle_chat_send(p, state).await,
        Method::ChatAbort(p) => handle_chat_abort(p, state).await,
        Method::ApprovalReply(p) => {
            state.resolve_approval(&p.approval_id, p.allow);
            Ok(MethodOk::Empty)
        }
        Method::SessionReset => Ok(MethodOk::Empty),
        Method::Status => Ok(MethodOk::Status(snapshot())),
        Method::Health => Ok(MethodOk::Health(oc_proto::HealthOk {
            ok: true,
            db_version: oc_store::migrate::TARGET_VERSION,
        })),
        Method::ChatHistory(_) => Ok(MethodOk::History(vec![])),
        Method::TasksList => Ok(MethodOk::Tasks(state.ledger().list())),
        Method::TasksCancel(p) => {
            state.ledger().cancel(&p.task_id);
            Ok(MethodOk::Empty)
        }
        // 以下方法在后续里程碑实现。
        Method::CronAdd(_)
        | Method::CronList
        | Method::CronRm(_)
        | Method::MemorySearch(_) => Err(ProtoError {
            kind: oc_proto::ErrorKind::Unsupported,
            message: "该方法将在后续里程碑实现".to_string(),
        }),
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

fn handle_connect(p: &ConnectParams) -> Result<MethodOk, ProtoError> {
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
        snapshot: snapshot(),
    })
}

/// M3：提交到主会话车道，返回分配的 run_id；实际处理经事件流推送。
async fn handle_chat_send(
    p: &ChatSendParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    match state.session().submit(p.text.clone()).await {
        Some(run_id) => Ok(MethodOk::ChatSend { run_id }),
        None => Err(ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: "会话车道不可用".to_string(),
        }),
    }
}

/// M3：中止活跃 run（hard 语义在 M4 完整）。
async fn handle_chat_abort(
    p: &ChatAbortParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    state.session().abort(p.run_id.clone(), p.hard).await;
    Ok(MethodOk::Empty)
}

fn snapshot() -> Snapshot {
    Snapshot {
        active_run: None,
        queued_turns: 0,
        background_tasks: 0,
        session: SessionId::main(),
    }
}
