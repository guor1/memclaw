//! HTTP server: POST /v1/responses, POST /v1/responses/:id/cancel.

use std::sync::Arc;

use axum::{
    extract::{Path, State as AxumState},
    response::{IntoResponse, Response as AxumResponse, Sse},
    routing::{get, post},
    Json, Router,
};
use dashmap::DashMap;
use tower_http::cors::CorsLayer;

use oc_proto::{
    ChatAbortParams, ChatSendParams, ConnectParams, Frame, Method, MethodOk, Req, ReqId, ResResult,
    RunId, SessionId, PROTO_VERSION,
};

use crate::{
    adapter::{self, ResponseSessions},
    conn_pool::{ConnPool, NdjsonConn},
    error::{HttpError, HttpResult},
    sse::{stream_sse, SseState},
    types::*,
};

/// What a `response_id` maps to: the session it ran in, and its run id
/// (needed to target `chat.abort`).
#[derive(Clone)]
pub struct ResponseRecord {
    pub session: SessionId,
    pub run_id: RunId,
}

#[derive(Clone)]
pub struct AppState {
    pool: ConnPool,
    /// response_id → session, for `previous_response_id` continuity.
    sessions: ResponseSessions,
    /// response_id → run record, for cancel.
    records: Arc<DashMap<String, ResponseRecord>>,
    default_model: String,
}

pub fn create_app(pool: ConnPool, default_model: String) -> Router {
    let state = AppState {
        pool,
        sessions: Arc::new(DashMap::new()),
        records: Arc::new(DashMap::new()),
        default_model,
    };

    Router::new()
        .route("/v1/responses", post(create_response))
        .route("/v1/responses/:id/cancel", post(cancel_response))
        .route("/health", get(health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// Perform the `connect` handshake. oc-server hard-rejects a version mismatch,
/// so this must precede any other method on a fresh connection.
async fn handshake(conn: &mut NdjsonConn) -> HttpResult<()> {
    send_req(
        conn,
        Method::Connect(ConnectParams {
            proto_version: PROTO_VERSION,
            token: None,
        }),
        None,
    )
    .await?;

    match await_res(conn).await? {
        MethodOk::Hello { .. } => Ok(()),
        other => Err(HttpError::Protocol(format!(
            "expected hello, got {other:?}"
        ))),
    }
}

/// Send a `Req` frame.
async fn send_req(
    conn: &mut NdjsonConn,
    method: Method,
    idempotency_key: Option<String>,
) -> HttpResult<()> {
    let frame = Frame::Req(Req {
        id: ReqId::new(uuid::Uuid::now_v7().to_string()),
        method,
        idempotency_key: idempotency_key.map(oc_proto::IdemKey::new),
    });
    conn.tx
        .send(frame)
        .await
        .map_err(|_| HttpError::Protocol("daemon connection closed".into()))
}

/// Read frames until the next `Res`, returning its payload.
///
/// Events may interleave before the `Res` arrives; they are dropped here since
/// no run is being tracked yet.
async fn await_res(conn: &mut NdjsonConn) -> HttpResult<MethodOk> {
    loop {
        match conn.rx.recv().await {
            Some(Frame::Res(res)) => {
                return match res.result {
                    ResResult::Ok(ok) => Ok(ok),
                    ResResult::Err(e) => Err(HttpError::from_proto(e)),
                }
            }
            Some(_) => continue,
            None => {
                return Err(HttpError::Protocol(
                    "daemon closed connection before responding".into(),
                ))
            }
        }
    }
}

async fn create_response(
    AxumState(state): AxumState<AppState>,
    Json(req): Json<CreateResponseReq>,
) -> HttpResult<AxumResponse> {
    adapter::reject_unsupported(&req)?;
    adapter::validate_tools(&req.tools)?;

    let session_id = adapter::resolve_session(&req, &state.sessions);
    let extracted = adapter::extract_input(&req)?;

    // Per-request instructions and file content ride along as a turn prefix.
    let prefix = adapter::build_turn_prefix(&extracted);
    let text = format!("{prefix}{}", extracted.text);

    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;

    send_req(
        &mut conn,
        Method::ChatSend(ChatSendParams {
            session: Some(session_id.clone()),
            text,
        }),
        Some(uuid::Uuid::now_v7().to_string()),
    )
    .await?;

    // The run id lets event filtering be exact rather than session-wide.
    let run_id = match await_res(&mut conn).await? {
        MethodOk::ChatSend { run_id } => run_id,
        other => {
            return Err(HttpError::Protocol(format!(
                "expected chat_send ok, got {other:?}"
            )))
        }
    };

    let response_id = format!("resp_{}", uuid::Uuid::now_v7());
    state
        .sessions
        .insert(response_id.clone(), session_id.clone());
    state.records.insert(
        response_id.clone(),
        ResponseRecord {
            session: session_id.clone(),
            run_id: run_id.clone(),
        },
    );

    let model = state.default_model.clone();
    let created_at = adapter::now_secs();

    if req.stream.unwrap_or(false) {
        let sse_state = SseState::new(response_id, run_id, model, created_at);
        let stream = stream_sse(conn, sse_state);
        Ok(Sse::new(stream)
            .keep_alive(
                axum::response::sse::KeepAlive::new()
                    .interval(std::time::Duration::from_secs(15)),
            )
            .into_response())
    } else {
        let response =
            accumulate_response(&mut conn, response_id, &run_id, model, created_at).await?;
        // Connection is at rest again (run reached a terminal phase), so it is
        // safe to hand back to the pool.
        state.pool.release(conn).await;
        Ok(Json(response).into_response())
    }
}

/// Drain events until the run reaches a terminal phase, then build the Response.
async fn accumulate_response(
    conn: &mut NdjsonConn,
    response_id: String,
    run_id: &RunId,
    model: String,
    created_at: i64,
) -> HttpResult<Response> {
    use oc_proto::{Event, LifecyclePhase};

    let mut text = String::new();
    let mut input_tokens = 0u32;
    let mut status = ResponseStatus::Completed;
    let mut error = None;

    loop {
        match conn.rx.recv().await {
            Some(Frame::Event(Event::Lifecycle { run_id: rid, phase, .. })) if &rid == run_id => {
                match phase {
                    LifecyclePhase::Start => {}
                    LifecyclePhase::End => break,
                    LifecyclePhase::Error { message, kind } => {
                        status = match kind {
                            oc_proto::RunErrorKind::Aborted => ResponseStatus::Cancelled,
                            _ => ResponseStatus::Failed,
                        };
                        error = Some(ResponseError {
                            message,
                            code: error_code(kind).to_string(),
                        });
                        break;
                    }
                }
            }
            Some(Frame::Event(Event::Assistant { run_id: rid, delta, .. })) if &rid == run_id => {
                text.push_str(&delta);
            }
            Some(Frame::Event(Event::Usage { input_tokens: n, .. })) => {
                input_tokens = n;
            }
            Some(_) => {}
            None => {
                return Err(HttpError::Protocol(
                    "daemon closed connection before the run finished".into(),
                ))
            }
        }
    }

    let output_tokens = estimate_tokens(&text);
    let output = if text.is_empty() {
        vec![]
    } else {
        vec![OutputItem::Message {
            id: format!("msg_{}", uuid::Uuid::now_v7()),
            status: "completed".into(),
            role: "assistant".into(),
            content: vec![ContentPart::OutputText {
                text,
                annotations: vec![],
            }],
        }]
    };

    Ok(Response {
        id: response_id,
        object: "response".into(),
        created_at,
        status,
        completed_at: Some(adapter::now_secs()),
        error,
        output,
        usage: Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
        },
        model,
    })
}

/// Rough token estimate for output. The daemon's `Usage` event reports input
/// tokens only, so output is approximated the same way oc-server does (chars/4)
/// rather than reported as 0.
pub fn estimate_tokens(text: &str) -> u32 {
    ((text.chars().count() / 4).max(if text.is_empty() { 0 } else { 1 })) as u32
}

fn error_code(kind: oc_proto::RunErrorKind) -> &'static str {
    use oc_proto::RunErrorKind as K;
    match kind {
        K::Aborted => "cancelled",
        K::Failed => "internal_error",
        K::Panicked => "internal_error",
        K::LoopDetected => "loop_detected",
        K::Timeout => "timeout",
    }
}

async fn cancel_response(
    AxumState(state): AxumState<AppState>,
    Path(response_id): Path<String>,
) -> HttpResult<AxumResponse> {
    let record = state
        .records
        .get(&response_id)
        .map(|e| e.clone())
        .ok_or_else(|| HttpError::NotFound(format!("response not found: {response_id}")))?;

    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;

    send_req(
        &mut conn,
        Method::ChatAbort(ChatAbortParams {
            run_id: record.run_id,
            hard: true,
        }),
        None,
    )
    .await?;

    match await_res(&mut conn).await? {
        MethodOk::Empty => {
            state.pool.release(conn).await;
            Ok(Json(serde_json::json!({
                "id": response_id,
                "object": "response",
                "status": "cancelled",
            }))
            .into_response())
        }
        other => Err(HttpError::Protocol(format!(
            "expected empty ok, got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_zero_only_for_empty() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1, "non-empty text is at least 1 token");
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }
}
