//! OpenAI Responses API compatibility layer.
//!
//! Provides HTTP + SSE adapter over oc-server's NDJSON protocol, following
//! OpenClaw's design principles:
//! - `previous_response_id` → session reuse (not cross-session OutputItem reference)
//! - `instructions` → append to system prompt (not replace SOUL.md)
//! - `tools` → client-side function tools (not agent-executed)
//! - `input_file` → system prompt injection (not Message structure change)

pub mod types;
pub mod adapter;
pub mod server;
pub mod error;
pub mod sse;
pub mod conn_pool;

pub use error::{HttpError, HttpResult};
pub use server::create_app;
