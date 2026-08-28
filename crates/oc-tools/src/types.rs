//! 工具公共类型（设计 §6）。

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// 工具规格（进 prompt，供模型决定调用）。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema 描述参数。
    pub parameters: serde_json::Value,
}

/// 工具策略。
#[derive(Debug, Clone)]
pub struct ToolPolicy {
    /// 是否可能需要审批（exec 类）。
    pub may_need_approval: bool,
    /// 工具级超时（防卡死，设计 §10.1）。
    pub timeout: Duration,
    /// 是否支持转后台（process 类）。
    pub backgroundable: bool,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            may_need_approval: false,
            timeout: Duration::from_secs(120),
            backgroundable: false,
        }
    }
}

/// 工具输出。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// 净化后的结果文本（回喂给模型）。
    pub content: String,
    /// 是否成功。
    pub success: bool,
    /// 若转后台，返回任务句柄 id。
    pub background_task: Option<String>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), success: true, background_task: None }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), success: false, background_task: None }
    }
}

/// 工具执行上下文。
pub struct ToolCtx {
    /// 取消令牌（工具级超时 / 用户中止）。
    pub cancel: CancellationToken,
    /// 流式更新发送端（工具进展 → 事件）。
    pub emit: mpsc::UnboundedSender<String>,
    /// 审批门：exec 用，向 server 请求审批并等回执。
    pub approval: Option<ApprovalGate>,
}

impl ToolCtx {
    /// 便捷构造（无审批门、丢弃更新）。测试/简单场景用。
    pub fn detached(cancel: CancellationToken) -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self { cancel, emit: tx, approval: None }
    }

    /// 发一条流式更新（失败静默——没人订阅不阻塞）。
    pub fn update(&self, chunk: impl Into<String>) {
        let _ = self.emit.send(chunk.into());
    }
}

/// 审批门句柄：向 server 发起审批请求，等用户回执。
pub struct ApprovalGate {
    pub request: mpsc::UnboundedSender<ApprovalRequest>,
}

/// 一次审批请求。
pub struct ApprovalRequest {
    pub summary: String,
    pub command: String,
    pub reply: oneshot::Sender<ApprovalReply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalReply {
    Allow,
    Deny,
}

impl ApprovalGate {
    /// 请求审批并等待回执。通道断开视为拒绝（保守）。
    pub async fn ask(&self, summary: String, command: String) -> ApprovalReply {
        let (reply, rx) = oneshot::channel();
        if self.request.send(ApprovalRequest { summary, command, reply }).is_err() {
            return ApprovalReply::Deny;
        }
        rx.await.unwrap_or(ApprovalReply::Deny)
    }
}
