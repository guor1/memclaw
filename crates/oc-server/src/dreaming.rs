//! Dreaming 巩固调度（设计 §4.5(d)、§7.4）。
//!
//! 心跳 tick 周期性触发：从 store 取 episodic 候选 → oc-core 双门判定 →
//! 通过者就地巩固为 curated + 写审计。**判定纯在 core，读写在 store，调度在此**。
//! 失败绝不阻塞主会话（本函数只告警，不向上传播错误）。

use oc_core::dreaming::{dreaming_gate, DreamCandidate, DreamCfg};
use oc_core::memory::{Origin as CoreOrigin, Tier as CoreTier};
use tracing::{info, warn};

/// 单次巩固候选拉取上限。
const SCAN_LIMIT: i64 = 64;

/// 执行一轮 dreaming 巩固扫描。返回本轮巩固的记忆条数。
pub async fn scan(store: &oc_store::Store, now_secs: i64, cfg: &DreamCfg) -> usize {
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
    promoted
}
