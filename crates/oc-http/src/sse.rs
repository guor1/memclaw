//! SSE encoding: oc-proto event stream → OpenAI Responses SSE events.

use std::pin::Pin;

use futures_util::Stream;
use oc_proto::{Event, Frame, LifecyclePhase, RunId, SessionId};

use crate::conn_pool::NdjsonConn;
use crate::error::{HttpError, HttpResult};
use crate::types::*;

/// Streaming state. Tracks the ids and indices OpenAI clients require, plus the
/// accumulated text needed for the terminal `.done` events.
pub struct SseState {
    pub response_id: String,
    /// Events are matched on run id, which is unique per turn — matching on
    /// session alone would mix in concurrent or background runs.
    pub run_id: RunId,
    /// Session this run belongs to. Needed for `Usage`, which the daemon
    /// broadcasts with session scope but no run id.
    pub session: SessionId,
    pub model: String,
    pub created_at: i64,
    item_id: Option<String>,
    sequence_number: u64,
    text: String,
    input_tokens: u32,
}

impl SseState {
    pub fn new(
        response_id: String,
        run_id: RunId,
        session: SessionId,
        model: String,
        created_at: i64,
    ) -> Self {
        Self {
            response_id,
            run_id,
            session,
            model,
            created_at,
            item_id: None,
            sequence_number: 0,
            text: String::new(),
            input_tokens: 0,
        }
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.sequence_number;
        self.sequence_number += 1;
        seq
    }

    fn build_response(&self, status: ResponseStatus, completed_at: Option<i64>) -> Response {
        let output = match &self.item_id {
            Some(id) => vec![OutputItem::Message {
                id: id.clone(),
                status: match status {
                    ResponseStatus::Completed => "completed".into(),
                    ResponseStatus::InProgress => "in_progress".into(),
                    _ => "incomplete".into(),
                },
                role: "assistant".into(),
                content: vec![ContentPart::OutputText {
                    text: self.text.clone(),
                    annotations: vec![],
                }],
            }],
            None => vec![],
        };

        let output_tokens = crate::server::estimate_tokens(&self.text);
        Response {
            id: self.response_id.clone(),
            object: "response".into(),
            created_at: self.created_at,
            status,
            completed_at,
            error: None,
            output,
            usage: Usage {
                input_tokens: self.input_tokens,
                output_tokens,
                total_tokens: self.input_tokens + output_tokens,
            },
            model: self.model.clone(),
        }
    }
}

/// Convert one oc-proto event into zero or more SSE frames, advancing state.
///
/// Returns `(payload, is_terminal)`. A terminal event ends the stream.
pub fn event_to_sse(ev: &Event, state: &mut SseState) -> Option<(String, bool)> {
    match ev {
        Event::Lifecycle { run_id, phase, .. } if run_id == &state.run_id => match phase {
            LifecyclePhase::Start => {
                let seq = state.next_seq();
                let payload = format_event(&SseEvent::ResponseCreated {
                    response: state.build_response(ResponseStatus::InProgress, None),
                    sequence_number: seq,
                });
                Some((payload, false))
            }
            LifecyclePhase::End => {
                let mut parts = Vec::new();

                // Only emit item/text completion events if any text was produced.
                if let Some(item_id) = state.item_id.clone() {
                    let seq = state.next_seq();
                    parts.push(format_event(&SseEvent::OutputTextDone {
                        item_id: item_id.clone(),
                        output_index: 0,
                        content_index: 0,
                        text: state.text.clone(),
                        sequence_number: seq,
                    }));

                    let seq = state.next_seq();
                    parts.push(format_event(&SseEvent::OutputItemDone {
                        item_id: item_id.clone(),
                        output_index: 0,
                        item: OutputItem::Message {
                            id: item_id,
                            status: "completed".into(),
                            role: "assistant".into(),
                            content: vec![ContentPart::OutputText {
                                text: state.text.clone(),
                                annotations: vec![],
                            }],
                        },
                        sequence_number: seq,
                    }));
                }

                let seq = state.next_seq();
                parts.push(format_event(&SseEvent::ResponseCompleted {
                    response: state
                        .build_response(ResponseStatus::Completed, Some(crate::adapter::now_secs())),
                    sequence_number: seq,
                }));
                parts.push("data: [DONE]\n\n".to_string());

                Some((parts.concat(), true))
            }
            LifecyclePhase::Error { message, kind } => {
                let seq = state.next_seq();
                let mut payload = format_event(&SseEvent::ResponseFailed {
                    response_id: state.response_id.clone(),
                    error: ResponseError {
                        message: message.clone(),
                        code: match kind {
                            oc_proto::RunErrorKind::Aborted => "cancelled",
                            oc_proto::RunErrorKind::LoopDetected => "loop_detected",
                            oc_proto::RunErrorKind::Timeout => "timeout",
                            oc_proto::RunErrorKind::Truncated => "truncated",
                            _ => "internal_error",
                        }
                        .into(),
                    },
                    sequence_number: seq,
                });
                payload.push_str("data: [DONE]\n\n");
                Some((payload, true))
            }
        },
        Event::Assistant { run_id, delta, .. } if run_id == &state.run_id => {
            // The first delta opens the output item and its text content part.
            let first = state.item_id.is_none();
            let item_id = match &state.item_id {
                Some(id) => id.clone(),
                None => {
                    let id = format!("msg_{}", uuid::Uuid::now_v7());
                    state.item_id = Some(id.clone());
                    id
                }
            };
            state.text.push_str(delta);

            let mut parts = Vec::new();
            if first {
                let seq = state.next_seq();
                parts.push(format_event(&SseEvent::OutputItemAdded {
                    item_id: item_id.clone(),
                    output_index: 0,
                    item: OutputItem::Message {
                        id: item_id.clone(),
                        status: "in_progress".into(),
                        role: "assistant".into(),
                        content: vec![],
                    },
                    sequence_number: seq,
                }));

                let seq = state.next_seq();
                parts.push(format_event(&SseEvent::ContentPartAdded {
                    item_id: item_id.clone(),
                    output_index: 0,
                    content_index: 0,
                    part: ContentPart::OutputText {
                        text: String::new(),
                        annotations: vec![],
                    },
                    sequence_number: seq,
                }));
            }

            let seq = state.next_seq();
            parts.push(format_event(&SseEvent::OutputTextDelta {
                item_id,
                output_index: 0,
                content_index: 0,
                delta: delta.clone(),
                sequence_number: seq,
                logprobs: vec![],
            }));

            Some((parts.concat(), false))
        }
        // `Usage` stays on the daemon's global broadcast (see oc-server `sink.rs`:
        // only inline run events moved to the per-run channel), so this connection
        // sees usage from *every* session — the TUI, cron, other HTTP requests.
        // It must be filtered by session or an unrelated run's token count
        // overwrites this response's.
        //
        // Session is the finest scope available: `Event::Usage` carries no run id,
        // so concurrent runs within one session still cannot be told apart.
        // Narrowing that needs a run id in the protocol.
        Event::Usage {
            session,
            input_tokens,
            ..
        } if session == &state.session => {
            state.input_tokens = *input_tokens;
            None
        }
        _ => None,
    }
}

fn format_event(ev: &SseEvent) -> String {
    match serde_json::to_string(ev) {
        Ok(json) => format!("data: {json}\n\n"),
        Err(e) => {
            tracing::warn!(error = %e, "sse event serialize failed");
            String::new()
        }
    }
}

/// Pull the data values out of an already-framed SSE payload.
///
/// [`event_to_sse`] emits complete frames (and sometimes several in one string,
/// e.g. the terminal batch plus `[DONE]`). Axum wants the bare value instead —
/// it does the `data: ` framing itself. Lines that are not `data: ` frames are
/// dropped, so blank separators do not turn into empty events.
fn split_sse_data(payload: &str) -> Vec<String> {
    payload
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(str::to_string)
        .collect()
}

/// Drive the SSE stream off the daemon connection.
///
/// Takes `conn` by value: the generator owns it for the stream's lifetime, so
/// when axum drops the stream — normal end, error, or client disconnect — the
/// connection drops too and its pool permit is released. The streaming path
/// never calls `ConnPool::release`, so that drop is the *only* thing returning
/// the permit; keep it that way.
pub fn stream_sse(
    conn: NdjsonConn,
    state: SseState,
) -> Pin<Box<dyn Stream<Item = HttpResult<axum::response::sse::Event>> + Send>> {
    Box::pin(async_stream::stream! {
        let mut conn = conn;
        let mut state = state;

        loop {
            match conn.rx.recv().await {
                Some(Frame::Event(ev)) => {
                    if let Some((payload, terminal)) = event_to_sse(&ev, &mut state) {
                        // `event_to_sse` returns an already-framed payload (`data: ...\n\n`,
                        // possibly several frames at once). Axum's `Event::data()` adds its
                        // own `data: ` prefix, so handing it the framed text verbatim emits
                        // `data: data: {...}` — which no standard SSE client can parse.
                        // Split the payload back into individual data values and let axum
                        // do the framing exactly once.
                        for value in split_sse_data(&payload) {
                            yield Ok(axum::response::sse::Event::default().data(value));
                        }
                        if terminal {
                            break;
                        }
                    }
                }
                Some(_) => {}
                None => {
                    yield Err(HttpError::Protocol("daemon closed connection mid-stream".into()));
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_proto::SessionId;

    fn state() -> SseState {
        SseState::new(
            "resp_x".into(),
            RunId::new("run_1"),
            SessionId::main(),
            "m".into(),
            0,
        )
    }

    fn assistant(run: &str, delta: &str) -> Event {
        Event::Assistant {
            session: SessionId::main(),
            run_id: RunId::new(run),
            delta: delta.into(),
        }
    }

    fn lifecycle(run: &str, phase: LifecyclePhase) -> Event {
        Event::Lifecycle {
            session: SessionId::main(),
            run_id: RunId::new(run),
            phase,
        }
    }

    #[test]
    fn events_from_other_runs_are_ignored() {
        let mut s = state();
        assert!(event_to_sse(&assistant("run_other", "x"), &mut s).is_none());
        assert!(s.text.is_empty());
    }

    #[test]
    fn first_delta_opens_item_and_content_part() {
        let mut s = state();
        let (payload, terminal) = event_to_sse(&assistant("run_1", "Hi"), &mut s).unwrap();
        assert!(!terminal);
        assert!(payload.contains("response.output_item.added"));
        assert!(payload.contains("response.content_part.added"));
        assert!(payload.contains("response.output_text.delta"));

        // Second delta must not re-open the item.
        let (payload2, _) = event_to_sse(&assistant("run_1", "!"), &mut s).unwrap();
        assert!(!payload2.contains("output_item.added"));
        assert_eq!(s.text, "Hi!");
    }

    #[test]
    fn sequence_numbers_are_monotonic() {
        let mut s = state();
        event_to_sse(&lifecycle("run_1", LifecyclePhase::Start), &mut s);
        event_to_sse(&assistant("run_1", "a"), &mut s);
        event_to_sse(&assistant("run_1", "b"), &mut s);
        let (end, _) = event_to_sse(&lifecycle("run_1", LifecyclePhase::End), &mut s).unwrap();
        // created(0) + added(1) + part(2) + delta(3) + delta(4) + done(5,6) + completed(7)
        assert!(end.contains("\"sequence_number\":7"), "got: {end}");
    }

    #[test]
    fn end_emits_done_events_and_done_sentinel() {
        let mut s = state();
        event_to_sse(&assistant("run_1", "Hi!"), &mut s);
        let (payload, terminal) =
            event_to_sse(&lifecycle("run_1", LifecyclePhase::End), &mut s).unwrap();
        assert!(terminal);
        assert!(payload.contains("response.output_text.done"));
        assert!(payload.contains("response.output_item.done"));
        assert!(payload.contains("response.completed"));
        assert!(payload.trim_end().ends_with("data: [DONE]"));
    }

    #[test]
    fn end_without_any_text_still_completes() {
        let mut s = state();
        let (payload, terminal) =
            event_to_sse(&lifecycle("run_1", LifecyclePhase::End), &mut s).unwrap();
        assert!(terminal);
        assert!(!payload.contains("output_item.done"), "no item was ever opened");
        assert!(payload.contains("response.completed"));
    }

    #[test]
    fn error_maps_to_failed_and_terminates() {
        let mut s = state();
        let (payload, terminal) = event_to_sse(
            &lifecycle(
                "run_1",
                LifecyclePhase::Error {
                    message: "boom".into(),
                    kind: oc_proto::RunErrorKind::Timeout,
                },
            ),
            &mut s,
        )
        .unwrap();
        assert!(terminal);
        assert!(payload.contains("response.failed"));
        assert!(payload.contains("timeout"));
    }

    #[test]
    fn usage_updates_tokens_without_emitting() {
        let mut s = state();
        let out = event_to_sse(
            &Event::Usage {
                session: SessionId::main(),
                input_tokens: 42,
                context_window: 1000,
            },
            &mut s,
        );
        assert!(out.is_none(), "usage is folded into the final response");
        assert_eq!(s.input_tokens, 42);
    }

    #[test]
    fn usage_from_another_session_is_ignored() {
        // The daemon broadcasts Usage to every connection, so a TUI or cron run
        // reporting tokens must not overwrite this response's count.
        let mut s = state(); // session = main
        let out = event_to_sse(
            &Event::Usage {
                session: SessionId::new("http-user-deadbeef"),
                input_tokens: 9999,
                context_window: 1000,
            },
            &mut s,
        );
        assert!(out.is_none());
        assert_eq!(s.input_tokens, 0, "another session's usage must not leak in");
    }
}
