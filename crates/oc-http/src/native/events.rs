//! Ambient event stream: broadcast-channel events only.
//!
//! Inline run events (`Assistant`, `Lifecycle`, `Tool`, `Approval`,
//! `UserInput`) never appear here — they travel through `RunSink::Conn` to the
//! connection that issued `chat.send` and are streamed back on that POST
//! response. See `chat.rs` for that path.
//!
//! What this stream *does* carry:
//! - `Usage` — context-window fill after each turn, broadcast to all.
//! - `Proactive` — cron / intent / heartbeat notifications, broadcast to all.
//! - `Task` — background-task state changes, broadcast to all.

use std::pin::Pin;

use axum::{
    extract::State as AxumState,
    response::{sse, Sse},
};
use futures_util::Stream;
use oc_proto::{Event, Frame, Method, MethodOk};

use crate::error::{HttpError, HttpResult};
use crate::proto::{await_res, handshake, send_req};
use crate::server::AppState;

/// GET /api/v1/events  →  SSE stream of ambient events.
///
/// Opens a dedicated daemon connection and subscribes to its broadcast. The
/// connection is held for the lifetime of the stream; when the client
/// disconnects, axum drops the stream and the connection drop returns its
/// pool permit.
pub async fn stream(
    AxumState(state): AxumState<AppState>,
) -> HttpResult<Sse<Pin<Box<dyn Stream<Item = HttpResult<sse::Event>> + Send>>>> {
    // Status probe doubles as the handshake: we need a live connection and the
    // snapshot is sent as the first SSE event so clients have current state.
    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(&mut conn, Method::Status, None).await?;
    let snapshot = match await_res(&mut conn).await? {
        MethodOk::Status(s) => s,
        other => return Err(HttpError::Protocol(format!("expected Status, got {other:?}"))),
    };

    let snapshot_json = serde_json::to_string(&snapshot)
        .map_err(|e| HttpError::Internal(format!("snapshot serialize: {e}")))?;

    let s = Box::pin(async_stream::stream! {
        let mut conn = conn;

        // Send current snapshot as the first event so clients don't need a
        // separate /status call to bootstrap their state.
        yield Ok(sse::Event::default().event("status").data(snapshot_json));

        loop {
            match conn.rx.recv().await {
                Some(Frame::Event(ev)) => {
                    // Pass through only ambient (broadcast) events.
                    let is_ambient = matches!(
                        &ev,
                        Event::Usage { .. } | Event::Proactive { .. } | Event::Task { .. }
                    );
                    if !is_ambient {
                        continue;
                    }
                    let event_name = super::chat::event_name(&ev);
                    match serde_json::to_string(&ev) {
                        Ok(json) => yield Ok(sse::Event::default().event(event_name).data(json)),
                        Err(e) => tracing::warn!(error = %e, "events sse serialize failed"),
                    }
                }
                Some(_) => {}
                None => {
                    yield Err(HttpError::Protocol("daemon closed events connection".into()));
                    break;
                }
            }
        }
    }) as Pin<Box<dyn Stream<Item = HttpResult<sse::Event>> + Send>>;

    Ok(Sse::new(s).keep_alive(sse::KeepAlive::new().interval(std::time::Duration::from_secs(20))))
}
