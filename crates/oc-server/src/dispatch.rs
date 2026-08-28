//! 请求分发（设计 §7.2）。
//!
//! M2：处理 req → 返回 res，必要时广播 event。无 agent loop，`chat.send`
//! 以 **echo** 演示完整事件流（lifecycle → assistant delta → lifecycle end）。

use std::sync::Arc;

use oc_proto::{
    ChatSendParams, ConnectParams, Event, Features, LifecyclePhase, Method, MethodOk, ProtoError,
    Req, ResResult, RunId, SessionId, Snapshot, PROTO_VERSION,
};
use uuid::Uuid;

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
        Method::SessionReset => Ok(MethodOk::Empty),
        Method::Status => Ok(MethodOk::Status(snapshot())),
        Method::Health => Ok(MethodOk::Health(oc_proto::HealthOk {
            ok: true,
            db_version: oc_store::migrate::TARGET_VERSION,
        })),
        Method::ChatHistory(_) => Ok(MethodOk::History(vec![])),
        // 以下方法在后续里程碑实现。
        Method::ChatAbort(_)
        | Method::CronAdd(_)
        | Method::CronList
        | Method::CronRm(_)
        | Method::TasksList
        | Method::TasksCancel(_)
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

/// M2 echo：立即返回 run_id，并异步广播 start → assistant(echo) → end。
async fn handle_chat_send(
    p: &ChatSendParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    let run_id = RunId::new(Uuid::now_v7().to_string());
    let text = p.text.clone();
    let state = Arc::clone(state);
    let rid = run_id.clone();

    tokio::spawn(async move {
        state.emit(Event::Lifecycle {
            run_id: rid.clone(),
            phase: LifecyclePhase::Start,
        });
        // echo：把用户文本作为一个 assistant delta 回推（M3 换成真正的模型流）。
        state.emit(Event::Assistant {
            run_id: rid.clone(),
            delta: format!("echo: {text}"),
        });
        state.emit(Event::Lifecycle {
            run_id: rid,
            phase: LifecyclePhase::End,
        });
    });

    Ok(MethodOk::ChatSend { run_id })
}

fn snapshot() -> Snapshot {
    Snapshot {
        active_run: None,
        queued_turns: 0,
        background_tasks: 0,
        session: SessionId::main(),
    }
}
