//! 服务端推送事件流（设计 §2.3）。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, RunId, SessionId, TaskId, ToolCallId};
use crate::method::TaskState;

/// 服务端主动推送的事件。`tag = "event"`。
///
/// 每个变体都带 `session`，让多会话并发下 client 能把事件归属到正确的会话
/// （设计：多会话支持）。cron/heartbeat 等隔离子会话产生的事件归属 `main`。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// run 生命周期。
    Lifecycle {
        session: SessionId,
        run_id: RunId,
        phase: LifecyclePhase,
    },
    /// 流式回复增量。
    Assistant {
        session: SessionId,
        run_id: RunId,
        delta: String,
    },
    /// 工具活动。
    Tool {
        session: SessionId,
        run_id: RunId,
        call_id: ToolCallId,
        phase: ToolPhase,
    },
    /// ★主动提醒推送（cron/intent 触发）。归属 `main` 会话。
    Proactive {
        session: SessionId,
        kind: ProactiveKind,
        text: String,
        source: ProactiveSource,
    },
    /// 后台任务进展/完成。
    Task {
        session: SessionId,
        task_id: TaskId,
        update: TaskUpdate,
    },
    /// ★审批请求：server 请求用户批准一个动作（如危险命令）。
    /// client 收到后应向用户展示，并用 `approval.reply` 方法回执。
    Approval {
        session: SessionId,
        approval_id: ApprovalId,
        run_id: RunId,
        summary: String,
        command: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum LifecyclePhase {
    Start,
    End,
    Error { message: String, kind: RunErrorKind },
}

/// run 结束/异常的归一化分类（对应 core 的 RunOutcome）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunErrorKind {
    Aborted,
    Failed,
    Panicked,
    LoopDetected,
    Timeout,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ToolPhase {
    Start { name: String, args_preview: String },
    Update { chunk: String },
    End { status: ToolStatus },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Aborted,
    Backgrounded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProactiveKind {
    Reminder,
    Wake,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProactiveSource {
    Cron { cron_id: String },
    Intent { intent_id: String },
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskUpdate {
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
