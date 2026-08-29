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
            let mut msg = json!({"role": role});
            match m.role {
                // tool 结果消息：必带 tool_call_id 关联到发起的调用。
                MsgRole::Tool => {
                    msg["content"] = json!(m.content);
                    if let Some(id) = &m.tool_call_id {
                        msg["tool_call_id"] = json!(id);
                    }
                }
                // assistant 若发起工具调用：序列化 tool_calls；content 为空时置 null。
                MsgRole::Assistant if !m.tool_calls.is_empty() => {
                    msg["content"] = if m.content.is_empty() {
                        serde_json::Value::Null
                    } else {
                        json!(m.content)
                    };
                    // thinking 模式：把该轮的 reasoning_content 带回，否则 400。
                    if let Some(rc) = &m.reasoning {
                        msg["reasoning_content"] = json!(rc);
                    }
                    msg["tool_calls"] = json!(m
                        .tool_calls
                        .iter()
                        .map(|tc| json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.args,
                            },
                        }))
                        .collect::<Vec<_>>());
                }
                _ => {
                    msg["content"] = json!(m.content);
                }
            }
            messages.push(msg);
        }
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
        });
        // 关键：把可用工具告知模型，否则模型只能把工具语法当纯文本吐出。
        if !req.tools.is_empty() {
            body["tools"] = json!(req
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    },
                }))
                .collect::<Vec<_>>());
            body["tool_choice"] = json!("auto");
        }
        body
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
            let code = status.as_u16();
            // 读出错误 body 带进诊断——否则 400 只剩 "HTTP 400"，无从排查。
            let body = resp.text().await.unwrap_or_default();
            let detail = body.trim();
            tracing::warn!(status = code, body = %detail, "provider 返回非 2xx");
            return Err(classify_status(code, None, detail));
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
    // thinking 模式：reasoning_content 与 content 分离流出，累积后回喂时需带回。
    if let Some(rc) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
        if !rc.is_empty() {
            return Some(Delta::Reasoning(rc.to_string()));
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

fn classify_status(status: u16, retry_after: Option<u64>, detail: &str) -> ProviderErr {
    // 截断过长 body，避免污染日志/错误链。
    let d: String = detail.chars().take(500).collect();
    let msg = if d.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {d}")
    };
    match status {
        401 | 403 => ProviderErr::Auth(msg),
        429 => ProviderErr::RateLimited { retry_after_secs: retry_after },
        400..=499 => ProviderErr::Invalid(msg),
        _ => ProviderErr::Transient(msg),
    }
}

// 忽略 usage 的解析以保持 M3 精简；M5 需要时补。
#[allow(dead_code)]
fn parse_usage(_v: &serde_json::Value) -> Option<Usage> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ToolCallSpec, ToolSpec};

    fn req_with(messages: Vec<Message>, tools: Vec<ToolSpec>) -> ModelRequest {
        ModelRequest {
            model: "deepseek-chat".into(),
            system: Some("sys".into()),
            messages,
            tools,
            max_tokens: None,
            temperature: None,
        }
    }

    fn user(text: &str) -> Message {
        Message { role: MsgRole::User, content: text.into(), tool_call_id: None, tool_calls: vec![], reasoning: None }
    }

    #[test]
    fn tools_serialized_into_body() {
        let tools = vec![ToolSpec {
            name: "exec".into(),
            description: "运行命令".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let body = OpenAiProvider::body(&req_with(vec![user("现在几点")], tools));
        let arr = body["tools"].as_array().expect("tools 应存在");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "exec");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn parses_reasoning_content_delta() {
        let chunk = r#"{"choices":[{"delta":{"reasoning_content":"让我想想"}}]}"#;
        assert_eq!(parse_chunk(chunk), Some(Delta::Reasoning("让我想想".into())));
        // content 优先于 reasoning（正常回复片段）。
        let chunk2 = r#"{"choices":[{"delta":{"content":"答案"}}]}"#;
        assert_eq!(parse_chunk(chunk2), Some(Delta::Text("答案".into())));
    }

    #[test]
    fn no_tools_field_when_empty() {
        let body = OpenAiProvider::body(&req_with(vec![user("hi")], vec![]));
        assert!(body.get("tools").is_none(), "无工具时不应出现 tools 字段");
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn assistant_tool_call_and_result_roundtrip() {
        let messages = vec![
            user("现在几点"),
            Message {
                role: MsgRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_calls: vec![ToolCallSpec {
                    id: "call_1".into(),
                    name: "exec".into(),
                    args: r#"{"cmd":"date"}"#.into(),
                }],
                reasoning: Some("先查当前时间".into()),
            },
            Message {
                role: MsgRole::Tool,
                content: "2026-08-29".into(),
                tool_call_id: Some("call_1".into()),
                tool_calls: vec![],
                reasoning: None,
            },
        ];
        let body = OpenAiProvider::body(&req_with(messages, vec![]));
        let msgs = body["messages"].as_array().unwrap();
        // [0]=system, [1]=user, [2]=assistant(tool_calls), [3]=tool
        let asst = &msgs[2];
        assert_eq!(asst["role"], "assistant");
        assert!(asst["content"].is_null(), "空 content 应为 null");
        assert_eq!(asst["reasoning_content"], "先查当前时间", "thinking 内容应回喂");
        assert_eq!(asst["tool_calls"][0]["id"], "call_1");
        assert_eq!(asst["tool_calls"][0]["function"]["name"], "exec");
        assert_eq!(asst["tool_calls"][0]["function"]["arguments"], r#"{"cmd":"date"}"#);
        let tool = &msgs[3];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_1");
        assert_eq!(tool["content"], "2026-08-29");
    }
}
