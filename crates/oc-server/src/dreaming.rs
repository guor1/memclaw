//! Dreaming 巩固调度（设计 §4.5(d)、§7.4、§11.4）。
//!
//! 心跳 tick 周期性触发：从 store 取 episodic 候选 → oc-core 双门判定 →
//! 通过者就地巩固为 curated + 写审计 →（P1-4）跑**巩固模型轮**重写 MEMORY.md。
//! **判定纯在 core，读写在 store/本模块，调度在此**。
//! 失败绝不阻塞主会话（本函数只告警，不向上传播错误）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_core::dreaming::{
    build_consolidation_prompt, decide_write, dreaming_gate, DreamCandidate, DreamCfg, WritePlan,
    CONSOLIDATION_SYSTEM_PROMPT,
};
use oc_core::memory::{Origin as CoreOrigin, Tier as CoreTier};
use oc_llm::{Delta, Message, ModelRequest, MsgRole, Provider};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// 单次巩固候选拉取上限。
const SCAN_LIMIT: i64 = 64;

/// 巩固模型轮的墙钟上限（夜间后台任务，给足时间但不无限等）。
const CONSOLIDATE_TIMEOUT: Duration = Duration::from_secs(120);

/// 巩固模型轮所需上下文。`None` 时只做 DB 内 tier 提升（保持 P1-4 之前的行为）。
#[derive(Clone)]
pub struct ConsolidateCtx {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    /// `~/.oc/soul/` 目录；MEMORY.md 写在其下。
    pub soul_dir: PathBuf,
}

/// 执行一轮 dreaming 巩固扫描（不含模型轮）。返回本轮巩固的记忆条数。
///
/// 保留此签名供既有调用方/测试使用；要跑 MEMORY.md 重写请用 [`scan_with`]。
pub async fn scan(store: &oc_store::Store, now_secs: i64, cfg: &DreamCfg) -> usize {
    scan_with(store, now_secs, cfg, None).await
}

/// 执行一轮 dreaming 巩固扫描，`ctx` 非空时追加**巩固模型轮**重写 MEMORY.md（§11.4）。
pub async fn scan_with(
    store: &oc_store::Store,
    now_secs: i64,
    cfg: &DreamCfg,
    ctx: Option<&ConsolidateCtx>,
) -> usize {
    let rows = match store.writer().dream_candidates(SCAN_LIMIT).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "dreaming：取候选失败，跳过本轮");
            return 0;
        }
    };
    if rows.is_empty() {
        return 0;
    }

    // 映射为 core 候选（age 用 now - created_at；毫秒转秒）。
    let cands: Vec<DreamCandidate> = rows
        .iter()
        .map(|r| DreamCandidate {
            id: r.id.clone(),
            tier: match r.tier {
                oc_store::Tier::Curated => CoreTier::Curated,
                oc_store::Tier::Episodic => CoreTier::Episodic,
                oc_store::Tier::Prospective => CoreTier::Prospective,
                oc_store::Tier::Review => CoreTier::Review,
            },
            origin: match r.origin {
                oc_store::Origin::Owner => CoreOrigin::Owner,
                oc_store::Origin::Agent => CoreOrigin::Agent,
                oc_store::Origin::Untrusted => CoreOrigin::Untrusted,
                oc_store::Origin::System => CoreOrigin::System,
            },
            importance: r.importance,
            use_count: r.use_count.max(0) as u32,
            age_secs: (now_secs - r.created_at / 1000).max(0),
        })
        .collect();

    let consolidations = dreaming_gate(&cands, cfg);
    if consolidations.is_empty() {
        return 0;
    }

    // 通过双门 → 就地巩固 + 审计。逐条容错。
    let mut promoted = 0usize;
    for c in &consolidations {
        if let Err(e) = store.writer().promote_memory(c.id.clone()).await {
            warn!(id = %c.id, error = %e, "dreaming：巩固失败");
            continue;
        }
        if let Err(e) = store
            .writer()
            .write_audit(
                "dreaming".into(),
                "consolidate".into(),
                Some(c.id.clone()),
            )
            .await
        {
            warn!(id = %c.id, error = %e, "dreaming：审计写入失败");
        }
        promoted += 1;
    }
    if promoted > 0 {
        info!(promoted, "dreaming：本轮巩固完成");
    }

    // 巩固模型轮：把本轮巩固的条目并进 MEMORY.md（§11.4）。
    // 失败只告警——DB 内的 tier 提升已经成功，文件写不成不该让整轮算失败。
    if promoted > 0 {
        if let Some(ctx) = ctx {
            let texts: Vec<String> = consolidations
                .iter()
                .filter_map(|c| rows.iter().find(|r| r.id == c.id))
                .map(|r| r.text.clone())
                .collect();
            rewrite_memory_md(ctx, store, &texts).await;
        }
    }
    promoted
}

/// 巩固模型轮 + 乐观并发写 MEMORY.md（设计 §11.4）。
///
/// 流程：读文件算 hash → 模型重写 → **再读一次**算 hash → `core::decide_write` 判定
/// → 未变则原子 rename 覆盖；变了则退化 append-only（不吞掉用户/他人的改动）。
async fn rewrite_memory_md(ctx: &ConsolidateCtx, store: &oc_store::Store, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let path = ctx.soul_dir.join("MEMORY.md");

    // 生成前读一次：既作为模型输入（要合并而非丢弃旧内容），也作为并发基线。
    let before = read_or_empty(&path);
    let hash_before = content_hash(&before);

    let refs: Vec<&str> = items.iter().map(|s| s.as_str()).collect();
    let prompt = build_consolidation_prompt(&before, &refs);
    let Some(new_body) = run_model(ctx, &prompt).await else {
        warn!("dreaming：巩固模型轮无输出，MEMORY.md 保持不变");
        return;
    };

    // 落盘前再读一次，比对哈希判有无并发修改。
    let hash_now = content_hash(&read_or_empty(&path));
    let plan = decide_write(&hash_before, &hash_now);

    let result = match plan {
        WritePlan::Overwrite => {
            debug!("dreaming：MEMORY.md 未被并发修改，整体重写");
            atomic_write(&path, &ensure_trailing_newline(&new_body))
        }
        WritePlan::AppendOnly => {
            // 期间有人改过：覆盖会丢掉对方的修改，改为追加。
            warn!("dreaming：MEMORY.md 期间被修改，退化为追加（不覆盖）");
            append_section(&path, &new_body)
        }
    };

    match result {
        Ok(()) => {
            let action = match plan {
                WritePlan::Overwrite => "rewrite_memory_md",
                WritePlan::AppendOnly => "append_memory_md",
            };
            info!(?plan, items = items.len(), "dreaming：MEMORY.md 已更新");
            if let Err(e) = store
                .writer()
                .write_audit("dreaming".into(), action.into(), None)
                .await
            {
                warn!(error = %e, "dreaming：MEMORY.md 写入审计失败");
            }
        }
        Err(e) => warn!(error = %e, path = %path.display(), "dreaming：MEMORY.md 写入失败"),
    }
}

/// 跑一轮巩固模型调用，累积文本。超时/失败/空返回 None。
async fn run_model(ctx: &ConsolidateCtx, prompt: &str) -> Option<String> {
    let req = ModelRequest {
        model: ctx.model.clone(),
        system: Some(CONSOLIDATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message {
            role: MsgRole::User,
            content: prompt.to_string(),
            tool_call_id: None,
            tool_calls: vec![],
            reasoning: None,
        }],
        tools: Vec::new(), // 巩固轮不带工具。
        max_tokens: None,
        temperature: None,
    };
    let cancel = CancellationToken::new();
    let run = async {
        let mut stream = match ctx.provider.stream_chat(req, cancel.clone()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "dreaming：巩固模型调用失败");
                return String::new();
            }
        };
        let mut acc = String::new();
        while let Some(delta) = stream.next().await {
            match delta {
                Ok(Delta::Text(t)) => acc.push_str(&t),
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "dreaming：巩固流中断");
                    break;
                }
            }
        }
        acc
    };
    let out = match tokio::time::timeout(CONSOLIDATE_TIMEOUT, run).await {
        Ok(s) => s,
        Err(_) => {
            cancel.cancel();
            warn!("dreaming：巩固模型轮超时");
            return None;
        }
    };
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 读文件，不存在/读失败均返回空串（首次巩固时文件可能还没建）。
fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// 原子写：先写同目录 `.tmp`，再 rename 覆盖。
///
/// 同目录是关键——跨盘/跨文件系统的 rename 不保证原子。中途崩溃时原文件仍完整。
/// Windows 上 `fs::rename` 覆盖已存在文件会失败，故先删目标再 rename。
fn atomic_write(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, body)?;
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // 别留垃圾。
            Err(e)
        }
    }
}

/// 追加一节到文末（并发退化路径）。带分隔标记，便于人工辨认与后续合并。
fn append_section(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "\n<!-- dreaming 追加（检测到并发修改，未覆盖原文） -->")?;
    writeln!(f, "{}", body.trim())?;
    Ok(())
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

/// 内容哈希（FNV-1a，16 位十六进制）。仅用于「变没变」的比对，非安全用途。
fn content_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
