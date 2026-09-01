//! store 数据类型（设计 §3.3）。持久化层的输入/输出 DTO。
//!
//! 这些是 store 与上层之间的数据契约，不含策略（策略在 oc-core）。

/// 会话种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Main,
    Cron,
    Dreaming,
    Lane2,
}

impl SessionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionKind::Main => "main",
            SessionKind::Cron => "cron",
            SessionKind::Dreaming => "dreaming",
            SessionKind::Lane2 => "lane2",
        }
    }
}

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        }
    }

    pub fn from_str(s: &str) -> Role {
        match s {
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            "system" => Role::System,
            _ => Role::User,
        }
    }
}

/// 待写入的一条 transcript 记录。
#[derive(Debug, Clone)]
pub struct NewEntry {
    pub session_id: String,
    pub role: Role,
    pub content: String,
    pub tokens_est: i64,
}

/// 已存储的一条会话记录（session.list 用）。
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub kind: String,
    pub created_at: i64,
    /// 上下文起点（reset 推进），无则 0。
    pub reset_at: i64,
}

/// 已存储的一条 transcript 记录。
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    pub session_id: String,
    pub seq: i64,
    pub role: Role,
    pub content: String,
    pub tokens_est: i64,
    pub created_at: i64,
}

/// 记忆分层（设计 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Curated,
    Episodic,
    Prospective,
    Review,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Curated => "curated",
            Tier::Episodic => "episodic",
            Tier::Prospective => "prospective",
            Tier::Review => "review",
        }
    }
    pub fn from_str(s: &str) -> Tier {
        match s {
            "curated" => Tier::Curated,
            "prospective" => Tier::Prospective,
            "review" => Tier::Review,
            _ => Tier::Episodic,
        }
    }
}

/// 记忆来源（provenance，抗投毒，设计 §4.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Owner,
    Agent,
    Untrusted,
    System,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::Owner => "owner",
            Origin::Agent => "agent",
            Origin::Untrusted => "untrusted",
            Origin::System => "system",
        }
    }
    pub fn from_str(s: &str) -> Origin {
        match s {
            "owner" => Origin::Owner,
            "agent" => Origin::Agent,
            "system" => Origin::System,
            _ => Origin::Untrusted,
        }
    }
}

/// 待写入的一条记忆。
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub id: String,
    pub tier: Tier,
    pub origin: Origin,
    pub text: String,
    pub keywords: Option<String>,
    pub importance: f64,
    pub content_hash: String,
    /// 偏好主题（P1-3，设计 §4.5(e)）。`Some` = 该条是偏好，参与 supersede
    /// （同主题的新值就地替换旧值）；`None` = 普通记忆，走内容哈希去重。
    /// 由 `oc_core::memory::extract_pref_key` 判定，调用方填入。
    pub pref_key: Option<String>,
}

/// 待写入的一条定时任务。
#[derive(Debug, Clone)]
pub struct NewCron {
    pub id: String,
    pub expr: String,
    pub prompt: String,
    pub tz: String,
    /// 下次触发（unix 秒）；由 core::next_fire 算好传入。
    pub next_at: Option<i64>,
}

/// 已存储的一条定时任务。
#[derive(Debug, Clone)]
pub struct CronRow {
    pub id: String,
    pub expr: String,
    pub prompt: String,
    pub tz: String,
    pub next_at: Option<i64>,
    pub last_fired_at: Option<i64>,
    pub enabled: bool,
}

/// 待写入的一条 standing intent（事件型待办）。
///
/// keywords 类型层用 `Vec<String>`，落库拼成空格分隔 TEXT（schema `keywords TEXT`）。
/// cooldown/budget/expiry 为该条自己的 anti-nagging 参数；调用方（server）用全局
/// 配置默认或用户指定填入。expiry_at 为绝对 unix 秒；None = 不过期。
#[derive(Debug, Clone)]
pub struct NewStandingIntent {
    pub id: String,
    /// 触发后注入的提醒正文。
    pub text: String,
    /// 词法触发关键词（命中任一即触发）。
    pub keywords: Vec<String>,
    pub cooldown_secs: i64,
    pub budget: u32,
    /// 过期时间点（unix 秒）；None = 不过期。
    pub expiry_at: Option<i64>,
}

/// 已存储的一条 standing intent（含触发判定所需的运行时状态）。
#[derive(Debug, Clone)]
pub struct StandingIntentRow {
    pub id: String,
    pub text: String,
    pub keywords: Vec<String>,
    pub cooldown_secs: i64,
    pub budget: u32,
    pub fired_count: u32,
    pub last_fired_at: Option<i64>,
    pub expiry_at: Option<i64>,
    pub created_at: i64,
}

/// 已存储的记忆（含检索所需字段）。
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub id: String,
    pub tier: Tier,
    pub origin: Origin,
    pub text: String,
    pub importance: f64,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub use_count: i64,
    pub content_hash: String,
    /// 偏好主题；`None` = 非偏好类记忆。见 [`NewMemory::pref_key`]。
    pub pref_key: Option<String>,
}
