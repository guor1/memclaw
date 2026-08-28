//! Anthropic 兼容 provider（feature `anthropic`）。`messages` 流式。
//!
//! Anthropic SSE 用具名事件（`content_block_delta` 等），归一到统一 [`Delta`]。

use futures_util::stream::{BoxStream, StreamExt};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::error::{LlmResult, ProviderErr};
use crate::provider::Provider;
use crate::sse::SseBuffer;
use crate::types::{Delta, FinishReason, ModelRequest, MsgRole};

pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    version: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com/v1".to_string()),
            version: "2023-06-01".to_string(),
            client: reqwest::Client::new(),
        }
    }

    fn body(req: &ModelRequest) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    // Anthropic 只有 user/assistant；system 单独字段，tool 结果作为 user。
                    MsgRole::Assistant => "assistant",
                    _ => "user",
                };
                json!({"role": role, "content": m.content})
            })
            .collect();
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "max_tokens": req.max_tokens.unwrap_or(4096),
        });
        if let Some(sys) = &req.system {
            body["system"] = json!(sys);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        body
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn stream_chat(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> LlmResult<BoxStream<'static, LlmResult<Delta>>> {
        let url = format!("{}/messages", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .json(&Self::body(&req))
            .send()
            .await
            .map_err(|e| ProviderErr::Transient(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => ProviderErr::Auth(format!("HTTP {status}")),
                429 => ProviderErr::RateLimited { retry_after_secs: None },
                400..=499 => ProviderErr::Invalid(format!("HTTP {status}")),
                _ => ProviderErr::Transient(format!("HTTP {status}")),
            });
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
                                    if let Some(d) = parse_event(&payload) {
                                        let is_done = matches!(d, Delta::Done(_));
                                        yield Ok(d);
                                        if is_done { return; }
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

/// 解析 Anthropic SSE data 负载为 Delta。
fn parse_event(payload: &str) -> Option<Delta> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    match v.get("type")?.as_str()? {
        "content_block_delta" => {
            let text = v.get("delta")?.get("text")?.as_str()?;
            Some(Delta::Text(text.to_string()))
        }
        "message_delta" => {
            // 结束原因在 delta.stop_reason。
            let reason = v.get("delta").and_then(|d| d.get("stop_reason")).and_then(|r| r.as_str());
            match reason {
                Some("tool_use") => Some(Delta::Done(FinishReason::ToolUse)),
                Some("max_tokens") => Some(Delta::Done(FinishReason::Length)),
                Some(_) => Some(Delta::Done(FinishReason::Stop)),
                None => None,
            }
        }
        "message_stop" => Some(Delta::Done(FinishReason::Stop)),
        _ => None,
    }
}
