//! message 工具（设计 §6.1）：主动向用户发一条消息（不等回复）。
//!
//! 与 `ask_user`（阻塞等答复）不同，`message` 是单向通知：模型把要告诉用户的话
//! 通过本工具发出，内容既作为流式 update 推给 client，也作为工具结果回喂模型。

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ToolResult};
use crate::types::{ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

pub struct MessageTool;

#[derive(Deserialize)]
struct MessageArgs {
    text: String,
}

#[async_trait]
impl Tool for MessageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "message".to_string(),
            description: "主动向用户发送一条消息（通知，不等待回复）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "要发给用户的消息" }
                },
                "required": ["text"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: false,
            timeout: std::time::Duration::from_secs(10),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: MessageArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        if args.text.trim().is_empty() {
            return Err(ToolError::BadArgs("text 不能为空".into()));
        }
        // 推给用户（作为工具 update 事件流出）。
        cx.update(args.text.clone());
        Ok(ToolOutput::ok(format!("已向用户发送消息：{}", args.text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn sends_message() {
        let tool = MessageTool;
        let cx = ToolCtx::detached(CancellationToken::new());
        let out = tool
            .invoke(serde_json::json!({"text": "任务完成"}), cx)
            .await
            .unwrap();
        assert!(out.success);
        assert!(out.content.contains("任务完成"));
    }

    #[tokio::test]
    async fn rejects_empty() {
        let tool = MessageTool;
        let cx = ToolCtx::detached(CancellationToken::new());
        let r = tool.invoke(serde_json::json!({"text": "  "}), cx).await;
        assert!(r.is_err());
    }
}
