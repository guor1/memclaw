//! ask_user 工具（设计 §6.1）：主动向用户提问，阻塞 run 等自由文本答复。
//!
//! 与 `message`（单向通知，不等回复）不同，`ask_user` 会**阻塞**当前 run，
//! 经输入门（[`InputGate`]）向 server 请求用户输入，等用户回执后把文本回喂模型。
//!
//! 取消语义与 exec 审批门同源（P0-2）：等待期间必须响应 cancel，否则用户 abort /
//! 看门狗判卡死时 run 会永久卡在 `ask().await`，车道不释放。

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ToolResult};
use crate::types::{ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

pub struct AskUserTool;

#[derive(Deserialize)]
struct AskUserArgs {
    /// 要问用户的问题。
    prompt: String,
}

#[async_trait]
impl Tool for AskUserTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".to_string(),
            description: "向用户提出一个问题并等待其回答。用于澄清歧义、缺失信息或需用户决策时。\
                          会阻塞直到用户作答；用户可能取消不答。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "要问用户的问题" }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            // 需交互回执通道（与审批同源）；无输入门时保守失败。
            may_need_approval: true,
            // 等人回答可能很久：超时给足（10 分钟），真正的兜底是 cancel/看门狗。
            timeout: std::time::Duration::from_secs(600),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: AskUserArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        let prompt = args.prompt.trim().to_string();
        if prompt.is_empty() {
            return Err(ToolError::BadArgs("prompt 不能为空".into()));
        }

        let Some(gate) = &cx.input else {
            // 无输入门（如非交互场景）→ 无法提问，保守失败而非静默假装。
            return Ok(ToolOutput::err(
                "无法向用户提问：当前会话不支持交互式输入。",
            ));
        };

        // 等用户回答期间必须响应取消（P0-2）：否则 abort / 看门狗时车道不释放。
        let reply = tokio::select! {
            _ = cx.cancel.cancelled() => return Err(ToolError::Aborted),
            r = gate.ask(prompt.clone()) => r,
        };

        match reply {
            Some(text) if !text.trim().is_empty() => {
                Ok(ToolOutput::ok(format!("用户回答：{}", text.trim())))
            }
            // 用户取消/空答复：回喂模型一个明确信号，让它自行决定下一步。
            _ => Ok(ToolOutput::ok("用户未作答（已取消提问）。")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InputGate, InputRequest};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn ctx_with_input(cancel: CancellationToken) -> (ToolCtx, mpsc::UnboundedReceiver<InputRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut cx = ToolCtx::detached(cancel);
        cx.input = Some(InputGate { request: tx });
        (cx, rx)
    }

    #[tokio::test]
    async fn returns_user_answer() {
        let (cx, mut rx) = ctx_with_input(CancellationToken::new());
        // 后台模拟用户作答。
        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert_eq!(req.prompt, "你叫什么？");
            let _ = req.reply.send(Some("Kiro".into()));
        });
        let out = AskUserTool
            .invoke(serde_json::json!({"prompt": "你叫什么？"}), cx)
            .await
            .unwrap();
        assert!(out.success);
        assert!(out.content.contains("Kiro"));
    }

    #[tokio::test]
    async fn cancel_returns_aborted() {
        let cancel = CancellationToken::new();
        let (cx, _rx) = ctx_with_input(cancel.clone());
        cancel.cancel(); // 立即取消
        let r = AskUserTool
            .invoke(serde_json::json!({"prompt": "在吗？"}), cx)
            .await;
        assert!(matches!(r, Err(ToolError::Aborted)));
    }

    #[tokio::test]
    async fn empty_answer_reports_no_reply() {
        let (cx, mut rx) = ctx_with_input(CancellationToken::new());
        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            let _ = req.reply.send(None); // 用户取消
        });
        let out = AskUserTool
            .invoke(serde_json::json!({"prompt": "?"}), cx)
            .await
            .unwrap();
        assert!(out.success);
        assert!(out.content.contains("未作答"));
    }

    #[tokio::test]
    async fn no_gate_reports_unsupported() {
        let cx = ToolCtx::detached(CancellationToken::new());
        let out = AskUserTool
            .invoke(serde_json::json!({"prompt": "?"}), cx)
            .await
            .unwrap();
        assert!(!out.success);
    }

    #[tokio::test]
    async fn rejects_empty_prompt() {
        let (cx, _rx) = ctx_with_input(CancellationToken::new());
        let r = AskUserTool.invoke(serde_json::json!({"prompt": "  "}), cx).await;
        assert!(r.is_err());
    }
}
