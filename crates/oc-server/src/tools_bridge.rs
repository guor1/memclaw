//! 工具执行桥（设计 §6 / §7.3）。
//!
//! 把 oc-tools 的 registry 包成 run driver 用的 `ToolExecutor`：
//! - 分派工具、套工具级超时
//! - 工具的流式更新 → `tool(update)` 事件
//! - 审批门：M4 首版用 config 的 ApprovalMode（危险命令直接拒/放行），
//!   交互式审批 UI 在紧接的补丁里接入

use std::sync::Arc;

use oc_llm::ToolSpec as LlmToolSpec;
use oc_proto::{ApprovalId, Event, RunId, SessionId, ToolCallId, ToolPhase, ToolStatus};
use oc_tools::types::{ApprovalGate, ApprovalReply, ToolCtx};
use oc_tools::{ToolError, ToolRegistry};
use tokio_util::sync::CancellationToken;

use crate::sink::RunSink;
use crate::state::ApprovalRegistry;

/// 审批 registry entry 的 RAII 清理守卫（P0-3）。
///
/// 无论审批以何种方式退出（正常回执 / 超时 / abort / pump 被 abort），
/// drop 时都从 registry 移除对应 entry，杜绝长期运行下的单调泄漏。
struct ApprovalGuard {
    registry: ApprovalRegistry,
    id: ApprovalId,
}

impl Drop for ApprovalGuard {
    fn drop(&mut self) {
        self.registry.remove(&self.id);
    }
}

/// 发起一次审批：登记回执通道 → 发 `Approval` 事件到 sink → 等回执。
///
/// - 事件走 per-run `sink`（与工具/文本事件同序，P0-1）。
/// - 等待 `select!` 叠加 `cancel`：abort/看门狗触发时立即返回拒绝，不卡住（P0-2 server 侧）。
/// - `ApprovalGuard` 保证 registry entry 必被清理（P0-3）。
/// - 通道断开（client 掉线）保守视为拒绝。
async fn request_approval(
    registry: &ApprovalRegistry,
    sink: &RunSink,
    session: &SessionId,
    run_id: &RunId,
    summary: &str,
    command: &str,
    cancel: &CancellationToken,
) -> bool {
    let approval_id = ApprovalId::new(uuid::Uuid::now_v7().to_string());
    let (tx, rx) = tokio::sync::oneshot::channel();
    registry.insert(approval_id.clone(), tx);
    // 从此刻起，任何退出路径都经 guard 清理 registry。
    let _guard = ApprovalGuard {
        registry: registry.clone(),
        id: approval_id.clone(),
    };

    sink.send(Event::Approval {
        session: session.clone(),
        approval_id,
        run_id: run_id.clone(),
        summary: summary.to_string(),
        command: command.to_string(),
    })
    .await;

    tokio::select! {
        _ = cancel.cancelled() => false,
        r = rx => r.unwrap_or(false),
    }
}

/// 后台移交 receiver（可被 serve_with 取出接到台账）。
pub type HandoffReceiver =
    Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<oc_tools::process::BackgroundHandoff>>>>;

/// run driver 用的工具执行器。
#[derive(Clone)]
pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
    /// 待处理审批注册表（与 ServerState 共享；None = 无交互式审批）。
    approvals: Option<ApprovalRegistry>,
    /// process 工具的后台移交 receiver（serve_with 取出接台账）。
    handoff: Option<HandoffReceiver>,
    /// 每会话工作目录（cd 状态）。工具无状态，cwd 状态在此编排层。
    cwds: Arc<dashmap::DashMap<oc_proto::SessionId, std::path::PathBuf>>,
    /// 初始工作目录（会话首次用时的默认值）。
    initial_cwd: std::path::PathBuf,
}

impl ToolExecutor {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        let initial_cwd = std::env::current_dir().unwrap_or_default();
        Self {
            registry,
            approvals: None,
            handoff: None,
            cwds: Arc::new(dashmap::DashMap::new()),
            initial_cwd,
        }
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

    /// 注入审批注册表（交互式审批）。与 ServerState 共享同一 registry，
    /// `approval.reply` 经 state 唤醒等待方。
    pub fn with_approvals(mut self, approvals: ApprovalRegistry) -> Self {
        self.approvals = Some(approvals);
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
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        name: &str,
        args: &str,
        session: &SessionId,
        run_id: &RunId,
        call_id: &ToolCallId,
        cancel: CancellationToken,
        sink: &RunSink,
    ) -> (ToolStatus, String) {
        let Some(tool) = self.registry.get(name) else {
            return (ToolStatus::Error, format!("未知工具: {name}"));
        };

        let args_val: serde_json::Value = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => return (ToolStatus::Error, format!("参数解析失败: {e}")),
        };

        // 工具流式更新 → tool(update) 事件，经 per-run sink（背压，不丢）。
        let (emit_tx, mut emit_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let sink_pump = sink.clone();
        let sid = session.clone();
        let rid = run_id.clone();
        let cid = call_id.clone();
        let pump = tokio::spawn(async move {
            while let Some(chunk) = emit_rx.recv().await {
                sink_pump
                    .send(Event::Tool {
                        session: sid.clone(),
                        run_id: rid.clone(),
                        call_id: cid.clone(),
                        phase: ToolPhase::Update { chunk },
                    })
                    .await;
            }
        });

        let policy = tool.policy();

        // 审批门：若共享了 registry，建 ApprovalGate 并起后台任务把请求转成
        // 「发 Approval 事件到 sink → 等回执/取消」（P0-2 取消 + P0-3 清理内聚于此）。
        let (approval_gate, approval_pump) = if let Some(approvals) = &self.approvals {
            let (req_tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel();
            let approvals = approvals.clone();
            let sink_appr = sink.clone();
            let sid = session.clone();
            let rid = run_id.clone();
            let cancel_appr = cancel.clone();
            let pump = tokio::spawn(async move {
                while let Some(r) = req_rx.recv().await {
                    let oc_tools::types::ApprovalRequest { summary, command, reply } = r;
                    let allow = request_approval(
                        &approvals, &sink_appr, &sid, &rid, &summary, &command, &cancel_appr,
                    )
                    .await;
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

        // 取该会话当前工作目录（缺省 = 初始目录）。
        let cwd = self
            .cwds
            .get(session)
            .map(|e| e.clone())
            .unwrap_or_else(|| self.initial_cwd.clone());

        let cx = ToolCtx {
            cancel: cancel.clone(),
            emit: emit_tx,
            approval: approval_gate,
            cwd,
        };

        let result = tokio::time::timeout(policy.timeout, tool.invoke(args_val, cx)).await;
        pump.abort();
        if let Some(p) = approval_pump {
            p.abort();
        }

        match result {
            Ok(Ok(output)) => {
                // cd 等工具变更了工作目录 → 写回该会话状态。
                if let Some(new_cwd) = output.new_cwd {
                    self.cwds.insert(session.clone(), new_cwd);
                }
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
