//! 协议方法与返回（设计 §2.2）。MVP 最小集。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, CronId, MemoryId, RunId, SessionId, TaskId};

/// 请求方法。`tag = "method", content = "params"`。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Method {
    /// 建立连接，返回 features + 初始快照。
    Connect(ConnectParams),
    /// 发一条消息，立即返回 `{run_id}`。side-effecting，需幂等键。
    ChatSend(ChatSendParams),
    /// 打断（防卡死 / 用户主动停）。
    ChatAbort(ChatAbortParams),
    /// 拉历史。
    ChatHistory(HistoryParams),
    /// `/new` `/reset`：推进上下文起点（可指定会话，缺省 main）。
    SessionReset(SessionResetParams),
    /// 列出所有会话（多会话切换/浏览用）。
    SessionsList,
    /// 添加定时任务。side-effecting。
    CronAdd(CronAddParams),
    CronList,
    CronRm(CronRmParams),
    TasksList,
    TasksCancel(TaskCancelParams),
    /// 记忆检索（调试/自省）。
    MemorySearch(MemSearchParams),
    /// 审批回执：对 `Approval` 事件的应答。
    ApprovalReply(ApprovalReplyParams),
    Status,
    Health,
}

/// 方法成功返回，与 [`Method`] 一一对应。
///
/// 用**邻接标签**（`tag` + `content`）而非内部标签：内部标签无法序列化
/// "包着序列的 newtype 变体"（如 `Sessions(Vec<..>)` 会在运行时报错），
/// 邻接标签把载荷放进独立的 `data` 字段，规避该限制。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "ok", content = "data", rename_all = "snake_case")]
pub enum MethodOk {
    Hello { features: Features, snapshot: Snapshot },
    ChatSend { run_id: RunId },
    Empty,
    History(Vec<Entry>),
    CronAdd { cron_id: CronId },
    CronList(Vec<CronSpec>),
    Tasks(Vec<TaskView>),
    MemorySearch(Vec<MemHit>),
    Sessions(Vec<SessionView>),
    Status(Snapshot),
    Health(HealthOk),
}

// ── params ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConnectParams {
    pub proto_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChatSendParams {
    #[serde(default)]
    pub session: Option<SessionId>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionResetParams {
    /// 目标会话；缺省为 main。
    #[serde(default)]
    pub session: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChatAbortParams {
    pub run_id: RunId,
    /// soft: 先 drain 排队轮再中止；hard: 立即中止活跃 run。
    #[serde(default)]
    pub hard: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HistoryParams {
    #[serde(default)]
    pub session: Option<SessionId>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronAddParams {
    pub expr: String,
    pub prompt: String,
    pub tz: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronRmParams {
    pub cron_id: CronId,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskCancelParams {
    pub task_id: TaskId,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemSearchParams {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalReplyParams {
    pub approval_id: ApprovalId,
    /// true = 批准，false = 拒绝。
    pub allow: bool,
}

// ── 返回体 ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Features {
    pub ws_remote: bool,
    pub memory_vec: bool,
    pub sandbox: bool,
    pub proto_version: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    pub active_run: Option<RunId>,
    pub queued_turns: u32,
    pub background_tasks: u32,
    pub session: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HealthOk {
    pub ok: bool,
    pub db_version: u32,
}

/// 一条 transcript 记录（对外视图）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Entry {
    pub seq: i64,
    pub role: Role,
    pub content: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
}

/// 一个会话的对外视图（sessions.list 用）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionView {
    pub id: SessionId,
    pub kind: String,
    pub created_at: i64,
    /// 上下文起点（reset 推进），无则 0。
    pub reset_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronSpec {
    pub id: CronId,
    pub expr: String,
    pub prompt: String,
    pub tz: String,
    pub enabled: bool,
    pub next_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskView {
    pub id: TaskId,
    pub kind: String,
    pub state: TaskState,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemHit {
    pub id: MemoryId,
    pub tier: String,
    pub text: String,
    pub score: f32,
}
