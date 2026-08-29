//! 工具执行桥（设计 §6 / §7.3）。
//!
//! 把 oc-tools 的 registry 包成 run driver 用的 `ToolExecutor`：
//! - 分派工具、套工具级超时
//! - 工具的流式更新 → `tool(update)` 事件
//! - 审批门：M4 首版用 config 的 ApprovalMode（危险命令直接拒/放行），
//!   交互式审批 UI 在紧接的补丁里接入

use std::sync::Arc;

use oc_llm::ToolSpec as LlmToolSpec;
use oc_proto::{Event, RunId, SessionId, ToolCallId, ToolPhase, ToolStatus};
use oc_tools::types::{ApprovalGate, ApprovalReply, ToolCtx};
use oc_tools::{ToolError, ToolRegistry};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// 审批处理器：run driver 注入，把工具的审批请求转成 client 交互。
#[async_trait::async_trait]
pub trait ApprovalHandler: Send + Sync {
    /// 请求用户审批一个命令。返回是否批准。
    async fn request(&self, session: &SessionId, run_id: &RunId, summary: &str, command: &str) -> bool;
}

/// 基于事件流的审批处理器：发 `Approval` 事件给 client，等 `approval.reply` 回执。
pub struct EventApprovalHandler {
    events: broadcast::Sender<Event>,
    registry: crate::state::ApprovalRegistry,
}

impl EventApprovalHandler {
    pub fn new(events: broadcast::Sender<Event>, registry: crate::state::ApprovalRegistry) -> Self {
        Self { events, registry }
    }
}

#[async_trait::async_trait]
impl ApprovalHandler for EventApprovalHandler {
    async fn request(&self, session: &SessionId, run_id: &RunId, summary: &str, command: &str) -> bool {
        let approval_id = oc_proto::ApprovalId::new(uuid::Uuid::now_v7().to_string());
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.registry.insert(approval_id.clone(), tx);

        let _ = self.events.send(Event::Approval {
            session: session.clone(),
            approval_id: approval_id.clone(),
            run_id: run_id.clone(),
            summary: summary.to_string(),
            command: command.to_string(),
        });

        // 等回执；通道断开（client 掉线）视为拒绝（保守）。
        match rx.await {
            Ok(allow) => allow,
            Err(_) => {
                self.registry.remove(&approval_id);
                false
            }
        }
    }
}

/// 后台移交 receiver（可被 serve_with 取出接到台账）。
pub type HandoffReceiver =
    Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<oc_tools::process::BackgroundHandoff>>>>;

/// run driver 用的工具执行器。
#[derive(Clone)]
pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
    approval: Option<Arc<dyn ApprovalHandler>>,
    /// process 工具的后台移交 receiver（serve_with 取出接台账）。
    handoff: Option<HandoffReceiver>,
}

impl ToolExecutor {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry, approval: None, handoff: None }
    }

    /// 设置后台移交 receiver。
    pub fn with_handoff(
        mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<oc_tools::process::BackgroundHandoff>,
    ) -> Self {
        self.handoff = Some(Arc::new(tokio::sync::Mutex::new(Some(rx))));
        self
    }

    /// 取出后台移交 receiver（仅一次）。
    pub fn take_handoff(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<oc_tools::process::BackgroundHandoff>> {
        let h = self.handoff.as_ref()?;
        h.try_lock().ok()?.take()
    }

    /// 注入审批处理器（交互式审批）。
    pub fn with_approval(mut self, handler: Arc<dyn ApprovalHandler>) -> Self {
        self.approval = Some(handler);
        self
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
        session: &SessionId,
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
        let sid = session.clone();
        let rid = run_id.clone();
        let cid = call_id.clone();
        let pump = tokio::spawn(async move {
            while let Some(chunk) = emit_rx.recv().await {
                let _ = ev.send(Event::Tool {
                    session: sid.clone(),
                    run_id: rid.clone(),
                    call_id: cid.clone(),
                    phase: ToolPhase::Update { chunk },
                });
            }
        });

        let policy = tool.policy();

        // 审批门：若注入了 handler，建 ApprovalGate 并起后台任务把请求转给 handler。
        let (approval_gate, approval_pump) = if let Some(handler) = &self.approval {
            let (req_tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel();
            let handler = Arc::clone(handler);
            let sid = session.clone();
            let rid = run_id.clone();
            let pump = tokio::spawn(async move {
                while let Some(r) = req_rx.recv().await {
                    let oc_tools::types::ApprovalRequest { summary, command, reply } = r;
                    let allow = handler.request(&sid, &rid, &summary, &command).await;
                    let _ = reply.send(if allow {
                        ApprovalReply::Allow
                    } else {
                        ApprovalReply::Deny
                    });
                }
            });
            (Some(ApprovalGate { request: req_tx }), Some(pump))
        } else {
            (None, None)
        };

        let cx = ToolCtx {
            cancel: cancel.clone(),
            emit: emit_tx,
            approval: approval_gate,
        };

        let result = tokio::time::timeout(policy.timeout, tool.invoke(args_val, cx)).await;
        pump.abort();
        if let Some(p) = approval_pump {
            p.abort();
        }

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
