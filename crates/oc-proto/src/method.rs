//! 协议方法与返回（设计 §2.2）。MVP 最小集。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, CronId, InputId, MemoryId, RunId, SessionId, TaskId};

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
    /// `/compact`：把历史摘要成 checkpoint（可指定会话，缺省 main）。
    Compact(CompactParams),
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
    /// 用户输入回执：对 `UserInput` 事件（ask_user 工具）的自由文本应答。
    UserReply(UserReplyParams),
    Status,
    Health,
    /// 整机诊断快照（`oc debug`）：会话表 + 活跃 run + 队列 + 写线程健康。
    Diagnostics,
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
    Diagnostics(DiagnosticsSnapshot),
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
pub struct CompactParams {
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UserReplyParams {
    pub input_id: InputId,
    /// 用户输入的自由文本；`None`/空 = 用户取消（未作答）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
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
    /// 模型上下文窗口（token）。
    pub context_window: u32,
    /// 最近一轮 provider 报告的真实输入 token 数（已用上下文近似）；无则 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_tokens: Option<u32>,
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

// ── 诊断快照（oc debug）────────────────────────────────────────

/// 整机诊断快照：运行时状态的一次采样，用于定位「不回复 / 截断 / 卡死」等
/// 时序问题。纯观测数据，无副作用。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsSnapshot {
    /// daemon 运行时长（秒）。
    pub uptime_secs: u64,
    /// 当前所有会话的运行时状态。
    pub sessions: Vec<SessionDiag>,
    /// 写线程是否存活（ping 一次写线程确认）。
    pub store_writer_alive: bool,
    /// 当前事件订阅者数（活跃连接近似）。
    pub event_subscribers: usize,
    /// 采样时刻（unix ms），供 client 计算各 run 的实时 age。
    pub sampled_at: i64,
    pub proto_version: u16,
}

/// 单会话运行时诊断。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionDiag {
    pub session_id: SessionId,
    /// 排队等待的轮数（不含活跃）。
    pub queue_depth: usize,
    /// 当前活跃 run（无则 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<RunSnapshot>,
    /// 车道被占用起始时刻（unix ms）；长 run / compact 阻塞可一眼看出。无占用则 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_busy_since: Option<i64>,
    /// 本会话累计起过的 run 数。
    pub total_runs: u64,
    /// 最近一次 run 的结束原因（outcome / finish_reason 文本）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_finish_reason: Option<String>,
    /// 最近一次错误文本（若有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// 活跃 run 的运行时快照。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunSnapshot {
    pub run_id: RunId,
    /// 当前阶段（排队/等模型/流式/工具执行）。
    pub phase: RunPhase,
    /// run 起始时刻（unix ms）。
    pub started_at: i64,
    /// 最近一次收到模型 delta 的时刻（unix ms）；判断「卡在等模型」用。无则 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delta_at: Option<i64>,
    /// 已进行的工具调用轮数。
    pub tool_rounds: usize,
    /// 已累积的 assistant 文本长度（字符）；判断「有没有在出字」。
    pub acc_chars: usize,
}

/// run 阶段（诊断用，比 oc-core 的 RunState 更粗粒度且可跨 crate 传输）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    /// 已提交、尚未起步（仍在 append_entry / load_history 等准备阶段）。
    Starting,
    /// 已发起模型调用、等待响应。
    AwaitingModel,
    /// 正在接收模型流式输出。
    Streaming,
    /// 正在执行工具。
    ToolExec,
    /// 正在执行 compact 摘要（占用车道）。
    Compacting,
}
