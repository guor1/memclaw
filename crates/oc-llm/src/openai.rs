//! OpenAI 兼容 provider（feature `openai`）。`chat/completions` 流式。
//!
//! 把 OpenAI 的 SSE chunk 归一到统一 [`Delta`]。

use futures_util::stream::{BoxStream, StreamExt};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::error::{LlmResult, ProviderErr};
use crate::provider::Provider;
use crate::sse::SseBuffer;
use crate::types::{Delta, FinishReason, ModelRequest, MsgRole, ToolCallDelta, Usage};

pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            client: reqwest::Client::new(),
        }
    }

    fn body(req: &ModelRequest) -> serde_json::Value {
        let mut messages: Vec<serde_json::Value> = Vec::new();
        if let Some(sys) = &req.system {
            messages.push(json!({"role": "system", "content": sys}));
        }
        for m in &req.messages {
            let role = match m.role {
                MsgRole::System => "system",
                MsgRole::User => "user",
                MsgRole::Assistant => "assistant",
                MsgRole::Tool => "tool",
            };
            messages.push(json!({"role": role, "content": m.content}));
        }
        json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
        })
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    async fn stream_chat(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> LlmResult<BoxStream<'static, LlmResult<Delta>>> {
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&Self::body(&req))
            .send()
            .await
            .map_err(|e| ProviderErr::Transient(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(classify_status(status.as_u16(), None));
        }

        let mut byte_stream = resp.bytes_stream();
        let stream = async_stream::stream! {
            let mut sse = SseBuffer::new();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        yield Err(ProviderErr::Cancelled);
                        break;
                    }
                    chunk = byte_stream.next() => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                for payload in sse.push(&bytes) {
                                    if payload == "[DONE]" {
                                        yield Ok(Delta::Done(FinishReason::Stop));
                                        return;
                                    }
                                    if let Some(d) = parse_chunk(&payload) {
                                        yield Ok(d);
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                yield Err(ProviderErr::Transient(e.to_string()));
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }
        };
        Ok(stream.boxed())
    }
}

/// 解析一条 OpenAI SSE chunk 为 Delta（仅取文本增量与结束原因）。
fn parse_chunk(payload: &str) -> Option<Delta> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let choice = v.get("choices")?.get(0)?;
    if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
        return Some(Delta::Done(match reason {
            "tool_calls" => FinishReason::ToolUse,
            "length" => FinishReason::Length,
            _ => FinishReason::Stop,
        }));
    }
    let delta = choice.get("delta")?;
    if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            return Some(Delta::Text(text.to_string()));
        }
    }
    if let Some(tc) = delta.get("tool_calls").and_then(|t| t.get(0)) {
        let call_id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
        let name = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_string());
        let args = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
            .unwrap_or("")
            .to_string();
        return Some(Delta::ToolCall(ToolCallDelta {
            call_id,
            name,
            args_chunk: args,
        }));
    }
    debug!("跳过无法识别的 chunk");
    None
}

fn classify_status(status: u16, retry_after: Option<u64>) -> ProviderErr {
    match status {
        401 | 403 => ProviderErr::Auth(format!("HTTP {status}")),
        429 => ProviderErr::RateLimited { retry_after_secs: retry_after },
        400..=499 => ProviderErr::Invalid(format!("HTTP {status}")),
        _ => ProviderErr::Transient(format!("HTTP {status}")),
    }
}

// 忽略 usage 的解析以保持 M3 精简；M5 需要时补。
#[allow(dead_code)]
fn parse_usage(_v: &serde_json::Value) -> Option<Usage> {
    None
}
