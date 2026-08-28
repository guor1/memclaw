//! 具体 SQL 操作（设计 §3.4）。同步 rusqlite 函数，写线程与读连接共用。
//!
//! 半衰期等浮点排名不在此处（策略在 oc-core）；这里只取候选集、做词法粗筛。

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::StoreResult;
use crate::types::{Entry, NewEntry, NewMemory, MemoryRow, Role, Tier, Origin};

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 确保 session 存在（不存在则建）。
pub fn ensure_session(conn: &Connection, id: &str, kind: &str) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO session(id, kind, created_at) VALUES(?1, ?2, ?3)
         ON CONFLICT(id) DO NOTHING",
        params![id, kind, now_millis()],
    )?;
    Ok(())
}

/// 追加一条 entry，seq 在会话内单调递增。返回新行 id。
pub fn append_entry(conn: &Connection, e: &NewEntry) -> StoreResult<i64> {
    // 下一个 seq = 当前最大 + 1（同会话）。
    let next_seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM entry WHERE session_id = ?1",
            params![e.session_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);

    conn.execute(
        "INSERT INTO entry(session_id, seq, role, content, tokens_est, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            e.session_id,
            next_seq,
            e.role.as_str(),
            e.content,
            e.tokens_est,
            now_millis()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 重置会话：推进 reset_at 到当前最大 seq（上下文起点前移，transcript 保留）。
pub fn reset_session(conn: &Connection, id: &str) -> StoreResult<()> {
    let max_seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM entry WHERE session_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    conn.execute(
        "UPDATE session SET reset_at = ?2 WHERE id = ?1",
        params![id, max_seq],
    )?;
    Ok(())
}

/// 加载会话历史（reset 之后的 entry，按 seq 升序），带最大条数限制。
///
/// token 预算裁剪由上层做（这里只按条数上限拉取，避免一次拉爆）。
pub fn load_transcript(conn: &Connection, session_id: &str, max_entries: i64) -> StoreResult<Vec<Entry>> {
    let reset_at: i64 = conn
        .query_row(
            "SELECT COALESCE(reset_at, 0) FROM session WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);

    // 取最近 max_entries 条（seq > reset_at），再正序返回。
    let mut stmt = conn.prepare(
        "SELECT id, session_id, seq, role, content, tokens_est, created_at
         FROM entry
         WHERE session_id = ?1 AND seq > ?2
         ORDER BY seq DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![session_id, reset_at, max_entries], |r| {
        Ok(Entry {
            id: r.get(0)?,
            session_id: r.get(1)?,
            seq: r.get(2)?,
            role: Role::from_str(&r.get::<_, String>(3)?),
            content: r.get(4)?,
            tokens_est: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    let mut out: Vec<Entry> = rows.collect::<Result<_, _>>()?;
    out.reverse(); // 变回正序
    Ok(out)
}

/// upsert 一条记忆（按 id）。
pub fn upsert_memory(conn: &Connection, m: &NewMemory) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO memory(id, tier, origin, text, keywords, importance, created_at, content_hash)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
           text=excluded.text, keywords=excluded.keywords,
           importance=excluded.importance, content_hash=excluded.content_hash",
        params![
            m.id,
            m.tier.as_str(),
            m.origin.as_str(),
            m.text,
            m.keywords,
            m.importance,
            now_millis(),
            m.content_hash
        ],
    )?;
    Ok(())
}

/// 更新记忆的使用时间与计数（召回后调用，用于半衰期）。
pub fn touch_memory(conn: &Connection, id: &str, at: i64) -> StoreResult<()> {
    conn.execute(
        "UPDATE memory SET last_used_at = ?2, use_count = use_count + 1 WHERE id = ?1",
        params![id, at],
    )?;
    Ok(())
}

/// 词法候选检索：用关键词 LIKE 粗筛，取候选集（精确排名在 oc-core）。
///
/// M5：先用 LIKE；FTS5 索引在第 4 段接入以提升相关性。
pub fn search_candidates(
    conn: &Connection,
    query_terms: &[String],
    tier_filter: Option<Tier>,
    limit: i64,
) -> StoreResult<Vec<MemoryRow>> {
    // 构造 OR LIKE 条件（词法粗筛）。无词则取最近的。
    let mut sql = String::from(
        "SELECT id, tier, origin, text, importance, created_at, last_used_at, use_count, content_hash
         FROM memory WHERE 1=1",
    );
    if let Some(t) = tier_filter {
        sql.push_str(&format!(" AND tier = '{}'", t.as_str()));
    }
    if !query_terms.is_empty() {
        sql.push_str(" AND (");
        for (i, _) in query_terms.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            // 用参数绑定防注入。
            sql.push_str(&format!("text LIKE ?{}", i + 1));
        }
        sql.push(')');
    }
    sql.push_str(&format!(" ORDER BY created_at DESC LIMIT {limit}"));

    let mut stmt = conn.prepare(&sql)?;
    let like_params: Vec<String> = query_terms.iter().map(|t| format!("%{t}%")).collect();
    let param_refs: Vec<&dyn rusqlite::ToSql> =
        like_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

    let rows = stmt.query_map(param_refs.as_slice(), |r| {
        Ok(MemoryRow {
            id: r.get(0)?,
            tier: Tier::from_str(&r.get::<_, String>(1)?),
            origin: Origin::from_str(&r.get::<_, String>(2)?),
            text: r.get(3)?,
            importance: r.get(4)?,
            created_at: r.get(5)?,
            last_used_at: r.get(6)?,
            use_count: r.get(7)?,
            content_hash: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}
