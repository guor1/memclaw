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
            // 流式返回 token 用量（末尾附一个带 usage 的 chunk）。DeepSeek/OpenAI 支持。
            "stream_options": { "include_usage": true },
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
                                    // 原始 chunk 落 trace：排查「空回复 / 工具分片没被识别」时
                                    // 用 RUST_LOG=oc_llm=trace 看 provider 到底发了什么。
                                    tracing::trace!(payload = %payload, "raw chunk");
                                    // 一个 chunk 可能产出多个 Delta（内容 + usage + Done）。
                                    for d in parse_chunk(&payload) {
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

/// 解析一条 OpenAI SSE chunk 为若干 [`Delta`]。
///
/// **一个 chunk 可能同时携带内容/finish_reason 和顶层 usage**（DeepSeek 在
/// tool_calls 收尾时就把 `finish_reason:"tool_calls"` 与 `usage` 合并进同一个
/// chunk）。因此不能"命中一项就提前返回"，否则会吞掉同 chunk 的其它信号——
/// 历史 bug：usage 分支提前 return，吞掉了 `finish_reason:tool_calls`，导致工具
/// 永不执行、空回复。
///
/// 顺序约定：内容/工具分片 → usage → Done。`Done` 必须最后，因为上层收到
/// `Done(ToolUse)`/`Done(Stop)` 会立即结束本轮，其后的 Delta 会被丢弃。
fn parse_chunk(payload: &str) -> Vec<Delta> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
        return Vec::new();
    };
    let mut out: Vec<Delta> = Vec::new();
    let mut done: Option<Delta> = None;

    if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    out.push(Delta::Text(text.to_string()));
                }
            }
            // thinking 模式：reasoning_content 与 content 分离流出，累积后回喂时需带回。
            if let Some(rc) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                if !rc.is_empty() {
                    out.push(Delta::Reasoning(rc.to_string()));
                }
            }
            // 只取 tool_calls[index=0]：本轮架构一次执行一个工具，模型的并行
            // tool_call（index>=1）忽略，模型会在下一轮按需重新请求。避免把不同
            // index 的 arguments 分片拼成非法 JSON。
            if let Some(tc) = delta.get("tool_calls").and_then(|t| t.get(0)) {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                if idx == 0 {
                    let call_id =
                        tc.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
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
                    out.push(Delta::ToolCall(ToolCallDelta { call_id, name, args_chunk: args }));
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            done = Some(Delta::Done(match reason {
                "tool_calls" => FinishReason::ToolUse,
                "length" => FinishReason::Length,
                _ => FinishReason::Stop,
            }));
        }
    }

    // usage 在 Done 之前发（Done 会结束本轮）。
    if let Some(u) = parse_usage(&v) {
        out.push(Delta::Usage(u));
    }
    if let Some(d) = done {
        out.push(d);
    }

    if out.is_empty() {
        debug!(payload = %payload, "跳过无法识别的 chunk");
    }
    out
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

/// 从 chunk 顶层解析 `usage`（include_usage 开启时末尾 chunk 携带）。
/// 兼容 OpenAI 别名：prompt_tokens/completion_tokens。
fn parse_usage(v: &serde_json::Value) -> Option<Usage> {
    let u = v.get("usage")?;
    // usage 可能为 null（普通增量 chunk）。
    if u.is_null() {
        return None;
    }
    let input = u
        .get("prompt_tokens")
        .or_else(|| u.get("input_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let output = u
        .get("completion_tokens")
        .or_else(|| u.get("output_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    if input == 0 && output == 0 {
        return None;
    }
    Some(Usage { input_tokens: input, output_tokens: output })
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
    fn parses_usage_chunk() {
        // include_usage 末尾 chunk：choices 空 + 顶层 usage。
        let chunk = r#"{"choices":[],"usage":{"prompt_tokens":1234,"completion_tokens":56}}"#;
        assert_eq!(
            parse_chunk(chunk),
            vec![Delta::Usage(Usage { input_tokens: 1234, output_tokens: 56 })]
        );
        // 普通增量 chunk 的 usage 为 null → 不误判。
        let normal = r#"{"choices":[{"delta":{"content":"hi"}}],"usage":null}"#;
        assert_eq!(parse_chunk(normal), vec![Delta::Text("hi".into())]);
    }

    #[test]
    fn finish_reason_and_usage_in_same_chunk() {
        // 回归：DeepSeek 在 tool_calls 收尾时把 finish_reason 与 usage 合并进同一
        // chunk。历史 bug 是 usage 分支提前 return 吞掉了 finish_reason，导致工具
        // 永不执行、空回复。现在应同时产出 Usage + Done(ToolUse)，且 Done 在末尾。
        let chunk = r#"{"choices":[{"delta":{"content":"","reasoning_content":null},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1306,"completion_tokens":95}}"#;
        assert_eq!(
            parse_chunk(chunk),
            vec![
                Delta::Usage(Usage { input_tokens: 1306, output_tokens: 95 }),
                Delta::Done(FinishReason::ToolUse),
            ]
        );
    }

    #[test]
    fn only_first_tool_call_index_taken() {
        // 并行 tool_call：只取 index=0，index>=1 忽略（避免拼成非法 JSON）。
        let idx0 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"sys","arguments":""}}]}}]}"#;
        assert_eq!(
            parse_chunk(idx0),
            vec![Delta::ToolCall(ToolCallDelta {
                call_id: "call_a".into(),
                name: Some("sys".into()),
                args_chunk: String::new(),
            })]
        );
        // index=1 的并行调用整体忽略 → 空。
        let idx1 = r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"sys","arguments":""}}]}}]}"#;
        assert_eq!(parse_chunk(idx1), Vec::<Delta>::new());
    }

    #[test]
    fn body_includes_usage_option() {
        let body = OpenAiProvider::body(&req_with(vec![user("hi")], vec![]));
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn parses_reasoning_content_delta() {
        let chunk = r#"{"choices":[{"delta":{"reasoning_content":"让我想想"}}]}"#;
        assert_eq!(parse_chunk(chunk), vec![Delta::Reasoning("让我想想".into())]);
        // content 与 reasoning 可共存（正常回复片段）。
        let chunk2 = r#"{"choices":[{"delta":{"content":"答案"}}]}"#;
        assert_eq!(parse_chunk(chunk2), vec![Delta::Text("答案".into())]);
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
