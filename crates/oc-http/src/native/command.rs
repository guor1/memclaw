//! Command endpoint: route a `/...` slash command to the daemon's sole parser.

use axum::{extract::State as AxumState, Json};
use oc_proto::{CommandParams, Method, MethodOk, SessionId};
use serde::Deserialize;

use crate::error::HttpResult;
use crate::proto::{await_res, handshake, send_req};
use crate::server::AppState;

#[derive(Deserialize)]
pub struct CommandReq {
    #[serde(default)]
    pub session: Option<String>,
    pub text: String,
}

/// POST /api/v1/command  →  CommandResult
///
/// The client only decides "does this start with `/`" and forwards the line;
/// the daemon parses and executes it, so the TUI and Web UI share one command set.
pub async fn run(
    AxumState(state): AxumState<AppState>,
    Json(req): Json<CommandReq>,
) -> HttpResult<impl axum::response::IntoResponse> {
    if req.text.trim().is_empty() {
        return Err(crate::error::HttpError::BadRequest("text must not be empty".into()));
    }
    let session = req
        .session
        .as_deref()
        .map(crate::adapter::validate_session_key)
        .transpose()?
        .unwrap_or_else(SessionId::main);

    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::Command(CommandParams { session: Some(session), text: req.text }),
        None,
    )
    .await?;
    match await_res(&mut conn).await? {
        MethodOk::Command(result) => {
            state.pool.release(conn).await;
            Ok(Json(result))
        }
        other => Err(crate::error::HttpError::Protocol(format!("expected Command, got {other:?}"))),
    }
}
