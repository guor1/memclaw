//! 摘要式压缩执行（设计 §4 第 3 层）。调模型把旧对话总结成结构化 checkpoint。
//!
//! 纯策略（prompt 组装）在 oc-core::summary；本模块只负责一次性调模型的 IO，
//! 带超时 + 失败降级（返回 None，调用方回退到 drop）。

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_core::summary::{build_summary_prompt, SummaryMsg, SUMMARIZATION_SYSTEM_PROMPT};
use oc_llm::{Delta, Message, ModelRequest, MsgRole, Provider};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// 摘要模型调用的墙钟上限。
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(120);

/// 把给定消息总结成一段结构化摘要文本。失败/空返回 None（调用方降级）。
pub async fn summarize(
    provider: &Arc<dyn Provider>,
    model: &str,
    messages: &[Message],
) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    let sm: Vec<SummaryMsg> = messages
        .iter()
        .map(|m| SummaryMsg {
            role: role_str(m.role),
            content: m.content.as_str(),
        })
        .collect();
    let prompt = build_summary_prompt(&sm);

    let req = ModelRequest {
        model: model.to_string(),
        system: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message {
            role: MsgRole::User,
            content: prompt,
            tool_call_id: None,
            tool_calls: vec![],
            reasoning: None,
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
    };

    let cancel = CancellationToken::new();
    let run = async {
        let mut stream = match provider.stream_chat(req, cancel.clone()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "摘要压缩：调模型失败");
                return String::new();
            }
        };
        let mut acc = String::new();
        while let Some(delta) = stream.next().await {
            match delta {
                Ok(Delta::Text(t)) => acc.push_str(&t),
                Ok(Delta::Done(_)) => break,
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "摘要压缩：流错误");
                    break;
                }
            }
        }
        acc
    };

    let text = match tokio::time::timeout(SUMMARY_TIMEOUT, run).await {
        Ok(t) => t,
        Err(_) => {
            cancel.cancel();
            warn!("摘要压缩：超时");
            String::new()
        }
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn role_str(r: MsgRole) -> &'static str {
    match r {
        MsgRole::System => "system",
        MsgRole::User => "user",
        MsgRole::Assistant => "assistant",
        MsgRole::Tool => "tool",
    }
}
