//! Dreaming 双门判定纯策略（设计 §4.5(d)、§9 巩固）。
//!
//! 夜间/空闲时，server 把 episodic 沉淀候选喂进 `dreaming_gate`，通过双门的候选
//! 交给"巩固模型轮"重写 MEMORY.md（该轮由 server 发起，本函数不做任何 IO）。
//!
//! - **门1（确定性排名门）**：分数（importance）/ 频次（use_count）/ 时间窗（age）三条硬阈值。
//! - **门2（结构排除门）**：`origin ∈ {Untrusted, System}` 直接排除，绝不巩固进 curated。
//!
//! 纯函数：相同输入 → 相同输出，100% 可单测。失败/空集绝不阻塞主会话（由 server 保证）。

use crate::memory::{Origin, Tier};

/// 一条待巩固候选（从 store 取回的字段映射而来）。
#[derive(Debug, Clone)]
pub struct DreamCandidate {
    pub id: String,
    pub tier: Tier,
    pub origin: Origin,
    /// 重要度 0..1。
    pub importance: f64,
    /// 被引用/命中的累计次数（频次门）。
    pub use_count: u32,
    /// 距创建时间的秒数（时间窗门）。
    pub age_secs: i64,
}

/// dreaming 判定配置。
#[derive(Debug, Clone)]
pub struct DreamCfg {
    /// 门1 分数下限：importance ≥ 此值。
    pub min_importance: f64,
    /// 门1 频次下限：use_count ≥ 此值（反复被用到才值得沉淀为长期记忆）。
    pub min_use_count: u32,
    /// 门1 时间窗下限：太新（还没沉淀稳定）不巩固。
    pub min_age_secs: i64,
    /// 门1 时间窗上限：太旧（已过时）不巩固；0 表示不设上限。
    pub max_age_secs: i64,
    /// 每次巩固轮的候选上限（成本/预算控制，设计 §9）。
    pub max_consolidations: usize,
}

impl Default for DreamCfg {
    fn default() -> Self {
        Self {
            min_importance: 0.5,
            min_use_count: 2,
            min_age_secs: 3 * 86400,      // 沉淀 ≥3 天
            max_age_secs: 180 * 86400,    // ≤180 天（更旧的让它自然半衰）
            max_consolidations: 8,
        }
    }
}

/// 被排除的原因（用于审计/可解释性）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateReject {
    /// tier 非 episodic（curated 已在册，无需巩固）。
    NotEpisodic,
    /// 门2：origin 属于 Untrusted/System，结构性排除。
    UntrustedOrigin,
    /// 门1：importance 低于阈值。
    LowImportance,
    /// 门1：频次不足。
    LowUseCount,
    /// 门1：太新，还没沉淀。
    TooRecent,
    /// 门1：太旧，已过时。
    TooOld,
}

/// 通过双门、待交给巩固模型轮的候选。
#[derive(Debug, Clone, PartialEq)]
pub struct Consolidation {
    pub id: String,
    /// 巩固优先级 = importance × use_count，越大越先重写。
    pub priority: f64,
}

/// 双门判定（设计 §4.5(d)）。
///
/// 依次过门1（分数/频次/时间窗）与门2（结构排除 Untrusted/System），
/// 通过者按 `priority = importance × use_count` 降序，截断到 `max_consolidations`。
pub fn dreaming_gate(cands: &[DreamCandidate], cfg: &DreamCfg) -> Vec<Consolidation> {
    let mut passed: Vec<Consolidation> = cands
        .iter()
        .filter(|c| gate_check(c, cfg).is_ok())
        .map(|c| Consolidation {
            id: c.id.clone(),
            priority: c.importance * c.use_count as f64,
        })
        .collect();
    passed.sort_by(|a, b| {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    passed.truncate(cfg.max_consolidations);
    passed
}

/// 巩固模型轮的系统提示词（设计 §11.4「巩固模型轮重写 MEMORY.md」）。
///
/// 要求模型**重写**而非追加：把零散记忆归并成条目清晰、无重复、无矛盾的清单。
/// 明确禁止编造——只允许重组输入里已有的事实，否则夜间无人监督的重写会往
/// 长期记忆里掺入幻觉，而 MEMORY.md 是每轮都注入的 curated 核心。
pub const CONSOLIDATION_SYSTEM_PROMPT: &str = "\
你在整理一份长期记忆清单（MEMORY.md）。把输入的零散记忆条目重写成一份干净的 Markdown 清单。

规则：
1. 只使用输入中已有的事实。**绝对不要**推断、扩写或编造任何未出现的信息。
2. 合并重复或高度相似的条目；同一主题有矛盾时保留**更具体/更近期**的表述。
3. 按主题分组，每组一个 `## 小标题`，组内用 `- ` 列出条目。
4. 每条尽量短，一行一件事。不要加前言、结语、解释或元评论。
5. 直接输出 Markdown 正文，不要包在代码块里。";

/// 组装巩固模型轮的用户提示（纯函数：给定条目 → 确定性 prompt）。
///
/// `existing` 是当前 MEMORY.md 正文（可空）；`items` 是本轮双门通过的记忆文本。
/// 既有内容一并交给模型，让它**合并**而不是只看新条目——否则重写会丢掉旧记忆。
pub fn build_consolidation_prompt(existing: &str, items: &[&str]) -> String {
    let mut s = String::new();
    if !existing.trim().is_empty() {
        s.push_str("## 当前 MEMORY.md 内容\n\n");
        s.push_str(existing.trim());
        s.push_str("\n\n");
    }
    s.push_str("## 本轮新巩固的记忆条目\n\n");
    for it in items {
        let t = it.trim();
        if !t.is_empty() {
            s.push_str("- ");
            s.push_str(t);
            s.push('\n');
        }
    }
    s.push_str("\n请把以上内容重写成一份合并去重后的完整清单。");
    s
}

/// MEMORY.md 的写入决策（设计 §11.4「写安全：乐观并发」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritePlan {
    /// 文件未被并发修改 → 可安全整体覆盖（原子 rename）。
    Overwrite,
    /// 文件在本轮生成期间被改动（用户手编 / 另一进程）→ **不覆盖**，
    /// 退化为追加到文末，避免吞掉别人的修改。
    AppendOnly,
}

/// 乐观并发判定（纯函数）：比对读取时与写入前的内容哈希。
///
/// 调用方在**生成前**读一次文件算 `hash_before`，模型生成完、落盘前**再读一次**算
/// `hash_now`。两者相同说明期间无人动过，可以整体重写；不同则说明有并发写入，
/// 此时覆盖会丢掉对方的改动，故退化为 append-only。
///
/// 纯函数不做 IO：哈希由调用方（server）算好传入。
pub fn decide_write(hash_before: &str, hash_now: &str) -> WritePlan {
    if hash_before == hash_now {
        WritePlan::Overwrite
    } else {
        WritePlan::AppendOnly
    }
}

/// 单候选双门检查：通过返回 `Ok(())`，否则返回**首个**未过的门（便于审计）。
pub fn gate_check(c: &DreamCandidate, cfg: &DreamCfg) -> Result<(), GateReject> {
    // 只巩固 episodic 沉淀（curated 已在册）。
    if c.tier != Tier::Episodic {
        return Err(GateReject::NotEpisodic);
    }
    // 门2：结构排除 —— 先于门1，来源不可信一票否决。
    if matches!(c.origin, Origin::Untrusted | Origin::System) {
        return Err(GateReject::UntrustedOrigin);
    }
    // 门1：分数 / 频次 / 时间窗。
    if c.importance < cfg.min_importance {
        return Err(GateReject::LowImportance);
    }
    if c.use_count < cfg.min_use_count {
        return Err(GateReject::LowUseCount);
    }
    if c.age_secs < cfg.min_age_secs {
        return Err(GateReject::TooRecent);
    }
    if cfg.max_age_secs > 0 && c.age_secs > cfg.max_age_secs {
        return Err(GateReject::TooOld);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, tier: Tier, origin: Origin, imp: f64, uses: u32, age: i64) -> DreamCandidate {
        DreamCandidate { id: id.into(), tier, origin, importance: imp, use_count: uses, age_secs: age }
    }

    fn good() -> DreamCandidate {
        // 满足所有门的基准候选。
        cand("ok", Tier::Episodic, Origin::Owner, 0.8, 5, 10 * 86400)
    }

    #[test]
    fn baseline_passes_both_gates() {
        let cfg = DreamCfg::default();
        assert!(gate_check(&good(), &cfg).is_ok());
        let out = dreaming_gate(&[good()], &cfg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "ok");
    }

    #[test]
    fn gate2_excludes_untrusted_and_system() {
        let cfg = DreamCfg::default();
        let untrusted = cand("u", Tier::Episodic, Origin::Untrusted, 0.9, 9, 10 * 86400);
        let system = cand("s", Tier::Episodic, Origin::System, 0.9, 9, 10 * 86400);
        assert_eq!(gate_check(&untrusted, &cfg), Err(GateReject::UntrustedOrigin));
        assert_eq!(gate_check(&system, &cfg), Err(GateReject::UntrustedOrigin));
        // 高分高频也无法绕过结构门。
        assert!(dreaming_gate(&[untrusted, system], &cfg).is_empty());
    }

    #[test]
    fn gate1_thresholds() {
        let cfg = DreamCfg::default();
        // 分数不足
        assert_eq!(
            gate_check(&cand("a", Tier::Episodic, Origin::Owner, 0.4, 5, 10 * 86400), &cfg),
            Err(GateReject::LowImportance)
        );
        // 频次不足
        assert_eq!(
            gate_check(&cand("b", Tier::Episodic, Origin::Owner, 0.8, 1, 10 * 86400), &cfg),
            Err(GateReject::LowUseCount)
        );
        // 太新
        assert_eq!(
            gate_check(&cand("c", Tier::Episodic, Origin::Owner, 0.8, 5, 86400), &cfg),
            Err(GateReject::TooRecent)
        );
        // 太旧
        assert_eq!(
            gate_check(&cand("d", Tier::Episodic, Origin::Owner, 0.8, 5, 365 * 86400), &cfg),
            Err(GateReject::TooOld)
        );
    }

    #[test]
    fn non_episodic_never_consolidated() {
        let cfg = DreamCfg::default();
        let curated = cand("cur", Tier::Curated, Origin::Owner, 0.9, 9, 10 * 86400);
        assert_eq!(gate_check(&curated, &cfg), Err(GateReject::NotEpisodic));
    }

    #[test]
    fn priority_orders_and_caps() {
        let cfg = DreamCfg { max_consolidations: 2, ..DreamCfg::default() };
        let cands = vec![
            cand("low", Tier::Episodic, Origin::Owner, 0.6, 2, 10 * 86400),   // prio 1.2
            cand("high", Tier::Episodic, Origin::Owner, 0.9, 8, 10 * 86400),  // prio 7.2
            cand("mid", Tier::Episodic, Origin::Owner, 0.8, 4, 10 * 86400),   // prio 3.2
        ];
        let out = dreaming_gate(&cands, &cfg);
        assert_eq!(out.len(), 2, "受 max_consolidations 截断");
        assert_eq!(out[0].id, "high");
        assert_eq!(out[1].id, "mid");
    }

    #[test]
    fn max_age_zero_means_no_upper_bound() {
        let cfg = DreamCfg { max_age_secs: 0, ..DreamCfg::default() };
        let ancient = cand("old", Tier::Episodic, Origin::Owner, 0.8, 5, 3650 * 86400);
        assert!(gate_check(&ancient, &cfg).is_ok());
    }

    #[test]
    fn decide_write_detects_concurrent_change() {
        // 哈希未变 → 期间无人动过，可整体重写。
        assert_eq!(decide_write("abc", "abc"), WritePlan::Overwrite);
        // 哈希变了 → 有并发写入，覆盖会吞掉对方改动，退化为追加。
        assert_eq!(decide_write("abc", "xyz"), WritePlan::AppendOnly);
    }

    #[test]
    fn consolidation_prompt_includes_existing_and_new() {
        let p = build_consolidation_prompt("## 旧\n- 老条目", &["新条目 A", "新条目 B"]);
        // 既有内容必须带上，否则重写会丢掉旧记忆。
        assert!(p.contains("老条目"), "应包含当前 MEMORY.md 内容: {p}");
        assert!(p.contains("新条目 A") && p.contains("新条目 B"));
        assert!(p.contains("重写"), "应给出重写指令");
    }

    #[test]
    fn consolidation_prompt_handles_empty_existing() {
        // 首次巩固（MEMORY.md 为空模板）不应出现空的"当前内容"节。
        let p = build_consolidation_prompt("   \n  ", &["条目"]);
        assert!(!p.contains("当前 MEMORY.md 内容"), "空既有内容应跳过该节: {p}");
        assert!(p.contains("条目"));
    }

    #[test]
    fn consolidation_prompt_is_deterministic() {
        let a = build_consolidation_prompt("x", &["1", "2"]);
        let b = build_consolidation_prompt("x", &["1", "2"]);
        assert_eq!(a, b, "同输入必须得同 prompt（可重现）");
    }

    #[test]
    fn consolidation_prompt_skips_blank_items() {
        let p = build_consolidation_prompt("", &["有效", "   ", ""]);
        assert!(p.contains("- 有效"));
        // 空条目不该产生空的 "- " 行。
        assert!(!p.contains("- \n"), "空白条目应被跳过: {p:?}");
    }
}
