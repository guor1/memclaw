//! Session management endpoints: list, history, reset, compact, status.

use axum::{
    extract::{Path, Query, State as AxumState},
    Json,
};
use oc_proto::{
    CompactParams, HistoryParams, Method, MethodOk, SessionId, SessionResetParams,
};
use serde::Deserialize;

use crate::error::HttpResult;
use crate::proto::{await_res, handshake, send_req};
use crate::server::AppState;

#[derive(Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<u32>,
}

/// GET /api/v1/sessions  →  Vec<SessionView>
pub async fn list(AxumState(state): AxumState<AppState>) -> HttpResult<impl axum::response::IntoResponse> {
    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(&mut conn, Method::SessionsList, None).await?;
    match await_res(&mut conn).await? {
        MethodOk::Sessions(sessions) => {
            state.pool.release(conn).await;
            Ok(Json(sessions))
        }
        other => Err(crate::error::HttpError::Protocol(format!("expected Sessions, got {other:?}"))),
    }
}

/// GET /api/v1/sessions/:id/history?limit=N  →  Vec<Entry>
pub async fn history(
    AxumState(state): AxumState<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> HttpResult<impl axum::response::IntoResponse> {
    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::ChatHistory(HistoryParams {
            session: Some(SessionId::new(id)),
            limit: q.limit,
        }),
        None,
    )
    .await?;
    match await_res(&mut conn).await? {
        MethodOk::History(entries) => {
            state.pool.release(conn).await;
            Ok(Json(entries))
        }
        other => Err(crate::error::HttpError::Protocol(format!("expected History, got {other:?}"))),
    }
}

/// POST /api/v1/sessions/:id/reset
pub async fn reset(
    AxumState(state): AxumState<AppState>,
    Path(id): Path<String>,
) -> HttpResult<impl axum::response::IntoResponse> {
    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::SessionReset(SessionResetParams { session: Some(SessionId::new(id)) }),
        None,
    )
    .await?;
    await_res(&mut conn).await?;
    state.pool.release(conn).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/v1/sessions/:id/compact
pub async fn compact(
    AxumState(state): AxumState<AppState>,
    Path(id): Path<String>,
) -> HttpResult<impl axum::response::IntoResponse> {
    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::Compact(CompactParams { session: Some(SessionId::new(id)) }),
        None,
    )
    .await?;
    await_res(&mut conn).await?;
    state.pool.release(conn).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/v1/status  →  Snapshot
pub async fn status(AxumState(state): AxumState<AppState>) -> HttpResult<impl axum::response::IntoResponse> {
    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(&mut conn, Method::Status, None).await?;
    match await_res(&mut conn).await? {
        MethodOk::Status(snap) => {
            state.pool.release(conn).await;
            Ok(Json(snap))
        }
        other => Err(crate::error::HttpError::Protocol(format!("expected Status, got {other:?}"))),
    }
}
