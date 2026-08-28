//! 工具执行桥（设计 §6 / §7.3）。
//!
//! 把 oc-tools 的 registry 包成 run driver 用的 `ToolExecutor`：
//! - 分派工具、套工具级超时
//! - 工具的流式更新 → `tool(update)` 事件
//! - 审批门：M4 首版用 config 的 ApprovalMode（危险命令直接拒/放行），
//!   交互式审批 UI 在紧接的补丁里接入

use std::sync::Arc;

use oc_llm::ToolSpec as LlmToolSpec;
use oc_proto::{Event, RunId, ToolCallId, ToolPhase, ToolStatus};
use oc_tools::types::ToolCtx;
use oc_tools::{ToolError, ToolRegistry};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// run driver 用的工具执行器。
#[derive(Clone)]
pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
}

impl ToolExecutor {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }

    /// 供 prompt/请求用的工具规格（转成 oc-llm 的 ToolSpec）。
    pub fn llm_specs(&self) -> Vec<LlmToolSpec> {
        self.registry
            .specs()
            .into_iter()
            .map(|s| LlmToolSpec {
                name: s.name,
                description: s.description,
                parameters: s.parameters,
            })
            .collect()
    }

    /// 执行一个工具。返回 (状态, 净化后的结果文本)。
    pub async fn run(
        &self,
        name: &str,
        args: &str,
        run_id: &RunId,
        call_id: &ToolCallId,
        cancel: CancellationToken,
        events: &broadcast::Sender<Event>,
    ) -> (ToolStatus, String) {
        let Some(tool) = self.registry.get(name) else {
            return (ToolStatus::Error, format!("未知工具: {name}"));
        };

        let args_val: serde_json::Value = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => return (ToolStatus::Error, format!("参数解析失败: {e}")),
        };

        // 工具流式更新 → tool(update) 事件。
        let (emit_tx, mut emit_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ev = events.clone();
        let rid = run_id.clone();
        let cid = call_id.clone();
        let pump = tokio::spawn(async move {
            while let Some(chunk) = emit_rx.recv().await {
                let _ = ev.send(Event::Tool {
                    run_id: rid.clone(),
                    call_id: cid.clone(),
                    phase: ToolPhase::Update { chunk },
                });
            }
        });

        let policy = tool.policy();
        let cx = ToolCtx {
            cancel: cancel.clone(),
            emit: emit_tx,
            // M4 首版：审批门在 exec 工具内部用 ApprovalMode 处理（config 派生）。
            // 交互式审批（弹给 client）在后续补丁接入。
            approval: None,
        };

        let result = tokio::time::timeout(policy.timeout, tool.invoke(args_val, cx)).await;
        pump.abort();

        match result {
            Ok(Ok(output)) => {
                let status = if output.success {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                };
                (status, output.content)
            }
            Ok(Err(ToolError::Denied)) => (ToolStatus::Error, "工具调用被审批拒绝".to_string()),
            Ok(Err(ToolError::Aborted)) => (ToolStatus::Aborted, "工具被中止".to_string()),
            Ok(Err(e)) => (ToolStatus::Error, format!("工具错误: {e}")),
            Err(_) => (ToolStatus::Error, "工具超时".to_string()),
        }
    }
}
