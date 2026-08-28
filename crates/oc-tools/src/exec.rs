//! exec 工具（设计 §6.1）：跑一次性命令/脚本。
//!
//! 审批门（危险命令弹审批）+ 工具级超时 + 取消 + 结果净化。

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use oc_core::tool::{approval_decision, classify_command, ApprovalMode, ApprovalOutcome};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::error::{ToolError, ToolResult};
use crate::sanitize::sanitize;
use crate::types::{ApprovalReply, ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

/// exec 工具。持有审批模式（从 config 派生）。
pub struct ExecTool {
    pub mode: ApprovalMode,
    pub timeout: Duration,
}

#[derive(Deserialize)]
struct ExecArgs {
    /// 要执行的命令行。
    command: String,
}

impl ExecTool {
    pub fn new(mode: ApprovalMode, timeout: Duration) -> Self {
        Self { mode, timeout }
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exec".to_string(),
            description: "执行一条 shell 命令并返回输出。危险命令会先请求用户审批。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令行" }
                },
                "required": ["command"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: true,
            timeout: self.timeout,
            backgroundable: true,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: ExecArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::BadArgs(e.to_string()))?;
        let cmd = args.command.trim().to_string();
        if cmd.is_empty() {
            return Err(ToolError::BadArgs("命令为空".into()));
        }

        // 审批门：分类 → 决策。
        let risk = classify_command(&cmd);
        match approval_decision(risk, self.mode) {
            ApprovalOutcome::Reject => return Err(ToolError::Denied),
            ApprovalOutcome::AskUser => {
                let Some(gate) = &cx.approval else {
                    // 无审批门却需要审批 → 保守拒绝。
                    return Err(ToolError::Denied);
                };
                let summary = format!("请求执行命令（风险: {risk:?}）");
                if gate.ask(summary, cmd.clone()).await == ApprovalReply::Deny {
                    return Err(ToolError::Denied);
                }
            }
            ApprovalOutcome::Execute => {}
        }

        cx.update(format!("$ {cmd}\n"));

        // 运行（超时 + 取消）。
        let child_fut = run_command(&cmd, &cx);
        let output = tokio::select! {
            _ = cx.cancel.cancelled() => return Err(ToolError::Aborted),
            r = tokio::time::timeout(self.timeout, child_fut) => {
                match r {
                    Ok(res) => res?,
                    Err(_) => return Err(ToolError::Timeout),
                }
            }
        };

        Ok(output)
    }
}

/// 实际跑命令：跨平台选 shell，流式收集 stdout+stderr。
async fn run_command(cmd: &str, cx: &ToolCtx) -> ToolResult<ToolOutput> {
    let mut command = shell_command(cmd);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let mut collected = String::new();

    // 逐行读 stdout 并流式 emit。
    if let Some(out) = stdout {
        let mut lines = BufReader::new(out).lines();
        while let Some(line) = lines.next_line().await? {
            cx.update(format!("{line}\n"));
            collected.push_str(&line);
            collected.push('\n');
        }
    }
    // stderr 追加。
    if let Some(err) = stderr {
        let mut lines = BufReader::new(err).lines();
        while let Some(line) = lines.next_line().await? {
            collected.push_str(&line);
            collected.push('\n');
        }
    }

    let status = child.wait().await?;
    let content = sanitize(&collected);
    Ok(ToolOutput {
        content: format!("{content}\n[退出码: {}]", status.code().unwrap_or(-1)),
        success: status.success(),
        background_task: None,
    })
}

/// 跨平台 shell 命令构造。
fn shell_command(cmd: &str) -> Command {
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    }
}
