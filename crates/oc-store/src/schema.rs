//! Schema DDL（设计 §3.3）。每个版本的迁移 SQL 集中在此，供 [`crate::migrate`] 逐步应用。

/// v1：MVP 全表（不含 sqlite-vec 虚表，虚表由 feature 单独创建）。
pub const V1: &str = r#"
-- ── 会话与转写 ──────────────────────────────────────────────
CREATE TABLE session (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL,
  created_at    INTEGER NOT NULL,
  reset_at      INTEGER
);

CREATE TABLE entry (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id    TEXT NOT NULL REFERENCES session(id),
  seq           INTEGER NOT NULL,
  role          TEXT NOT NULL,
  content       TEXT NOT NULL,
  tokens_est    INTEGER NOT NULL,
  -- 工具调用结构（P2-4）。缺了这两列，历史重放只能把工具结果降级成 user 文本，
  -- 模型在自己的上下文里从没见过「我发起工具调用」的样例，于是学会宣布完就等
  -- 用户贴结果（in-context learning 压倒系统提示词）。
  tool_calls    TEXT,          -- assistant 发起的调用（JSON 数组），NULL = 无
  tool_call_id  TEXT,          -- tool 结果关联的调用 id，NULL = 非工具结果
  created_at    INTEGER NOT NULL,
  UNIQUE(session_id, seq)
);
CREATE INDEX idx_entry_session_seq ON entry(session_id, seq);

-- ── 记忆索引 ───────────────────────────────────────────────
CREATE TABLE memory (
  id            TEXT PRIMARY KEY,
  tier          TEXT NOT NULL,
  origin        TEXT NOT NULL,
  text          TEXT NOT NULL,
  keywords      TEXT,
  importance    REAL NOT NULL DEFAULT 0.5,
  created_at    INTEGER NOT NULL,
  last_used_at  INTEGER,
  use_count     INTEGER NOT NULL DEFAULT 0,
  content_hash  TEXT NOT NULL,
  injected_mark INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_memory_tier_origin ON memory(tier, origin);

-- ── 主动性 ─────────────────────────────────────────────────
CREATE TABLE cron (
  id            TEXT PRIMARY KEY,
  expr          TEXT NOT NULL,
  prompt        TEXT NOT NULL,
  tz            TEXT NOT NULL,
  next_at       INTEGER,
  last_fired_at INTEGER,
  enabled       INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE standing_intent (
  id            TEXT PRIMARY KEY,
  text          TEXT NOT NULL,
  keywords      TEXT,
  trigger_vec   BLOB,
  scope         TEXT,
  cooldown_secs INTEGER NOT NULL DEFAULT 86400,
  budget        INTEGER NOT NULL DEFAULT 3,
  fired_count   INTEGER NOT NULL DEFAULT 0,
  last_fired_at INTEGER,
  expiry_at     INTEGER,
  created_at    INTEGER NOT NULL
);

-- ── 后台任务台账 ───────────────────────────────────────────
CREATE TABLE task (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL,
  state         TEXT NOT NULL,
  detail        TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

-- ── 审批 / 审计 ────────────────────────────────────────────
CREATE TABLE audit (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  at            INTEGER NOT NULL,
  actor         TEXT NOT NULL,
  action        TEXT NOT NULL,
  payload       TEXT,
  hash_prev     TEXT,
  hash_self     TEXT
);

-- ── 配置状态 ───────────────────────────────────────────────
CREATE TABLE kv (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);
"#;

/// v2：偏好主题列（P1-3，设计 §4.5(e) User model supersede）。
///
/// `supersede` 要按主题查同类既有偏好（"编辑器" 下已记了什么），故加 `pref_key`。
/// NULL = 非偏好类记忆（走原有的内容哈希去重路径），既有行升级后即为 NULL。
///
/// `ALTER TABLE ADD COLUMN` 是 SQLite 最安全的 DDL：不重建表、不动既有数据。
pub const V2: &str = r#"
ALTER TABLE memory ADD COLUMN pref_key TEXT;
CREATE INDEX idx_memory_pref_key ON memory(pref_key);
"#;

/// sqlite-vec 虚表（feature `sqlite-vec`）。维度随 embedding 模型，暂定 768。
#[cfg(feature = "sqlite-vec")]
pub const V1_VEC: &str = r#"
CREATE VIRTUAL TABLE memory_vec USING vec0(
  memory_id TEXT PRIMARY KEY,
  embedding FLOAT[768]
);
"#;
