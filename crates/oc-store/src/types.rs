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
}
