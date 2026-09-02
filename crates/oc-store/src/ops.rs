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

/// 列出所有会话，按创建时间倒序（最近的在前）。
pub fn session_list(conn: &Connection) -> StoreResult<Vec<crate::types::SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, created_at, COALESCE(reset_at, 0)
         FROM session
         ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::types::SessionRow {
            id: r.get(0)?,
            kind: r.get(1)?,
            created_at: r.get(2)?,
            reset_at: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
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
            role: Role::from_db_str(&r.get::<_, String>(3)?),
            content: r.get(4)?,
            tokens_est: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    let mut out: Vec<Entry> = rows.collect::<Result<_, _>>()?;
    out.reverse(); // 变回正序
    Ok(out)
}

/// 摘要式压缩：把 `seq <= up_to_seq` 的对话压成一条摘要 entry，并推进 reset_at
/// 到 up_to_seq（排除被摘要的原始区间，但保留 transcript 供审计）。
///
/// 事务内完成：插入 System 角色的摘要 entry（seq 在 max+1，落在 reset 之后故会被
/// 后续 load 取到）+ 更新 reset_at。摘要文本由 server 调模型生成后传入。
pub fn compact_with_summary(
    conn: &Connection,
    session_id: &str,
    up_to_seq: i64,
    summary_text: &str,
) -> StoreResult<()> {
    let tx = conn.unchecked_transaction()?;
    let content = format!("【上下文摘要】\n{summary_text}");
    let tokens_est = (content.chars().count() as i64 / 4).max(1);
    // 摘要 entry 追加在末尾（新 seq），落在 reset_at 之后。
    let next_seq: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM entry WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);
    tx.execute(
        "INSERT INTO entry(session_id, seq, role, content, tokens_est, created_at)
         VALUES(?1, ?2, 'system', ?3, ?4, ?5)",
        params![session_id, next_seq, content, tokens_est, now_millis()],
    )?;
    // 推进 reset_at 到 up_to_seq：排除原始被摘要区间，但摘要 entry(seq=next_seq)保留。
    tx.execute(
        "UPDATE session SET reset_at = ?2 WHERE id = ?1",
        params![session_id, up_to_seq],
    )?;
    tx.commit()?;
    Ok(())
}

/// upsert 一条记忆（按 id）。
pub fn upsert_memory(conn: &Connection, m: &NewMemory) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO memory(id, tier, origin, text, keywords, importance, created_at, content_hash, pref_key)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
           text=excluded.text, keywords=excluded.keywords,
           importance=excluded.importance, content_hash=excluded.content_hash,
           pref_key=excluded.pref_key",
        params![
            m.id,
            m.tier.as_str(),
            m.origin.as_str(),
            m.text,
            m.keywords,
            m.importance,
            now_millis(),
            m.content_hash,
            m.pref_key
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

/// 新增一条 cron 定时任务。
pub fn cron_add(conn: &Connection, c: &crate::types::NewCron) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO cron(id, expr, prompt, tz, next_at, enabled) VALUES(?1, ?2, ?3, ?4, ?5, 1)",
        params![c.id, c.expr, c.prompt, c.tz, c.next_at],
    )?;
    Ok(())
}

/// 列出所有 cron 任务。
pub fn cron_list(conn: &Connection) -> StoreResult<Vec<crate::types::CronRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, expr, prompt, tz, next_at, last_fired_at, enabled FROM cron ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::types::CronRow {
            id: r.get(0)?,
            expr: r.get(1)?,
            prompt: r.get(2)?,
            tz: r.get(3)?,
            next_at: r.get(4)?,
            last_fired_at: r.get(5)?,
            enabled: r.get::<_, i64>(6)? != 0,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 删除一条 cron 任务。返回是否删到行。
pub fn cron_rm(conn: &Connection, id: &str) -> StoreResult<bool> {
    let n = conn.execute("DELETE FROM cron WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// 触发后更新：记录 last_fired_at + 抬 fired_count（借 standing_intent 语义？
/// cron 无 fired_count 列，仅更 next_at / last_fired_at）。
pub fn cron_mark_fired(conn: &Connection, id: &str, fired_at: i64, next_at: Option<i64>) -> StoreResult<()> {
    conn.execute(
        "UPDATE cron SET last_fired_at = ?2, next_at = ?3 WHERE id = ?1",
        params![id, fired_at, next_at],
    )?;
    Ok(())
}


/// 当前 unix 秒。standing intent 的时间语义（created/last_fired/expiry）统一用秒，
/// 与 `oc_core::proactive::allow_fire`（秒级）对齐；不用 `now_millis`（那是 entry 语义）。
fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// keywords 落库编码：**换行分隔**。空列表 → 空串。
///
/// 不用空格分隔：关键词本身可能含空格（`oc intent add "带插头" "business trip"` 里
/// `business trip` 是**一个**关键词）。空格分隔会在取回时把它劈成两个，于是
/// `intent_prefilter` 单命中 "trip" 就触发——比用户要求的宽得多。换行不会出现在
/// 关键词里（编码时把内部空白归一为单空格兜底），故可安全作分隔符。
fn encode_keywords(kws: &[String]) -> String {
    kws.iter()
        // 内部空白归一为单空格：顺手清掉可能混入的换行，保证分隔符唯一。
        .map(|k| k.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|k| !k.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// keywords 取回解码：按换行切分，去空。含空格的关键词整条保留。
fn decode_keywords(s: Option<String>) -> Vec<String> {
    s.map(|s| {
        s.split('\n')
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// 新增一条 standing intent。created_at = now（秒）。
pub fn intent_add(conn: &Connection, i: &crate::types::NewStandingIntent) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO standing_intent(id, text, keywords, cooldown_secs, budget, fired_count, expiry_at, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
        params![
            i.id,
            i.text,
            encode_keywords(&i.keywords),
            i.cooldown_secs,
            i.budget,
            i.expiry_at,
            now_secs(),
        ],
    )?;
    Ok(())
}

/// 列出所有 standing intent，按 created_at 升序（早建的在前）。
pub fn intent_list(conn: &Connection) -> StoreResult<Vec<crate::types::StandingIntentRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, text, keywords, cooldown_secs, budget, fired_count, last_fired_at, expiry_at, created_at
         FROM standing_intent ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::types::StandingIntentRow {
            id: r.get(0)?,
            text: r.get(1)?,
            keywords: decode_keywords(r.get::<_, Option<String>>(2)?),
            cooldown_secs: r.get(3)?,
            budget: r.get::<_, i64>(4)? as u32,
            fired_count: r.get::<_, i64>(5)? as u32,
            last_fired_at: r.get(6)?,
            expiry_at: r.get(7)?,
            created_at: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 删除一条 standing intent。返回是否删到行。
pub fn intent_rm(conn: &Connection, id: &str) -> StoreResult<bool> {
    let n = conn.execute("DELETE FROM standing_intent WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// 触发后更新：抬 fired_count + 记 last_fired_at（秒）。
pub fn intent_mark_fired(conn: &Connection, id: &str, fired_at: i64) -> StoreResult<()> {
    conn.execute(
        "UPDATE standing_intent SET fired_count = fired_count + 1, last_fired_at = ?2 WHERE id = ?1",
        params![id, fired_at],
    )?;
    Ok(())
}

/// 取 dreaming 待巩固候选：episodic tier 的记忆（双门判定在 oc-core）。
///
/// 返回字段含 use_count / created_at / last_used_at，供 core 算频次/时间窗门。
/// 按 use_count 降序取前 `limit` 条（先看反复用到的）。
pub fn dream_candidates(conn: &Connection, limit: i64) -> StoreResult<Vec<MemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, tier, origin, text, importance, created_at, last_used_at, use_count, content_hash, pref_key
         FROM memory WHERE tier = 'episodic'
         ORDER BY use_count DESC, created_at ASC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |r| {
        Ok(MemoryRow {
            id: r.get(0)?,
            tier: Tier::from_db_str(&r.get::<_, String>(1)?),
            origin: Origin::from_db_str(&r.get::<_, String>(2)?),
            text: r.get(3)?,
            importance: r.get(4)?,
            created_at: r.get(5)?,
            last_used_at: r.get(6)?,
            use_count: r.get(7)?,
            content_hash: r.get(8)?,
            pref_key: r.get(9)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 巩固一条记忆：episodic → curated（dreaming 双门通过后调用）。
///
/// 就地提升 tier 并抬升 importance（下限 0.6），使其进入 curated 自动注入池。
pub fn promote_memory(conn: &Connection, id: &str) -> StoreResult<()> {
    conn.execute(
        "UPDATE memory SET tier = 'curated', importance = MAX(importance, 0.6)
         WHERE id = ?1 AND tier = 'episodic'",
        params![id],
    )?;
    Ok(())
}

/// 追加一条审计记录，维护哈希链（设计 §3.3 audit：hash_prev/hash_self）。
///
/// `hash_prev` = 上一条的 `hash_self`（无则空串）；
/// `hash_self` = 链哈希(at, actor, action, payload, hash_prev)。
/// 用非加密的 FNV-1a（tamper-evident 足够；单用户本地库不需抗碰撞）。
pub fn write_audit(
    conn: &Connection,
    actor: &str,
    action: &str,
    payload: Option<&str>,
) -> StoreResult<()> {
    let at = now_millis();
    let hash_prev: String = conn
        .query_row(
            "SELECT hash_self FROM audit ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();

    let material = format!("{at}|{actor}|{action}|{}|{hash_prev}", payload.unwrap_or(""));
    let hash_self = fnv1a_hex(&material);

    conn.execute(
        "INSERT INTO audit(at, actor, action, payload, hash_prev, hash_self)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![at, actor, action, payload, hash_prev, hash_self],
    )?;
    Ok(())
}

/// FNV-1a 64 位哈希，输出 16 位十六进制。非加密，仅用于审计链自洽校验。
fn fnv1a_hex(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
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
    // 列顺序必须与下方 query_map 的取值下标一致（含 pref_key 在第 9 位）。
    let mut sql = String::from(
        "SELECT id, tier, origin, text, importance, created_at, last_used_at, use_count, content_hash, pref_key
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
            tier: Tier::from_db_str(&r.get::<_, String>(1)?),
            origin: Origin::from_db_str(&r.get::<_, String>(2)?),
            text: r.get(3)?,
            importance: r.get(4)?,
            created_at: r.get(5)?,
            last_used_at: r.get(6)?,
            use_count: r.get(7)?,
            content_hash: r.get(8)?,
            pref_key: r.get(9)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 按偏好主题取既有偏好（P1-3，供 `supersede` 判同主题冲突）。
///
/// 只看 curated tier：偏好属于 curated（用户显式交代），episodic 的情节记忆
/// 不参与 supersede。按 created_at 升序（早建的在前，替换时优先命中最早那条）。
pub fn memory_by_pref_key(conn: &Connection, key: &str) -> StoreResult<Vec<MemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, tier, origin, text, importance, created_at, last_used_at, use_count, content_hash, pref_key
         FROM memory WHERE pref_key = ?1 AND tier = 'curated'
         ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![key], |r| {
        Ok(MemoryRow {
            id: r.get(0)?,
            tier: Tier::from_db_str(&r.get::<_, String>(1)?),
            origin: Origin::from_db_str(&r.get::<_, String>(2)?),
            text: r.get(3)?,
            importance: r.get(4)?,
            created_at: r.get(5)?,
            last_used_at: r.get(6)?,
            use_count: r.get(7)?,
            content_hash: r.get(8)?,
            pref_key: r.get(9)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 删除一条记忆（P1-3：supersede 的 Replace 用来清掉被取代的旧偏好）。
///
/// 返回是否删到行。
pub fn delete_memory(conn: &Connection, id: &str) -> StoreResult<bool> {
    let n = conn.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
    Ok(n > 0)
}
