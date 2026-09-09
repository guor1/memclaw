//! 上下文压缩策略（设计 §4，M5）。纯策略：只做判定与计划，不执行 IO。
//!
//! 三层手段，按代价从低到高：
//! 1. **工具结果剪枝**：历史里旧的大块工具输出先被截短（信息密度最低）
//! 2. **丢弃最早轮次**：仍超预算则从最早开始丢
//! 3. **摘要压缩**：需要保留语义时，标记待摘要区间交由 server 调模型生成摘要
//!
//! `plan_compaction` 是纯函数：给定消息的 token 估算与预算，返回一个计划。

/// 一条消息在压缩视角下的元信息（调用方从真实消息映射而来）。
#[derive(Debug, Clone, PartialEq)]
pub struct MsgMeta {
    /// 在原始消息序列中的下标。
    pub index: usize,
    /// 估算 token 数。
    pub tokens: i64,
    /// 是否为工具结果（剪枝优先级最高）。
    pub is_tool_result: bool,
    /// 该条 assistant 是否发起了工具调用。
    ///
    /// `assistant(tool_calls)` 与紧随其后的 `tool(result)` 是协议上的原子对，
    /// 拆开任一半都会让 provider 400，故丢弃时必须成对进出（见
    /// [`plan_compaction`] 第 2 步）。本项目每轮恒定只有一个工具调用且结果紧跟
    /// 其后入队，所以配对是位置相邻的，不必按 id 匹配。
    pub is_tool_dispatch: bool,
}

/// 压缩计划：server 按此执行。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompactPlan {
    /// 需要剪枝的工具结果下标（截短到 `prune_to_tokens`）。
    pub prune_tool_results: Vec<usize>,
    /// 需要整条丢弃的消息下标（最早的若干条）。
    pub drop: Vec<usize>,
    /// 建议摘要的区间 `[start, end)`（server 调模型生成摘要替换之）。
    /// 为 None 表示无需摘要。
    pub summarize: Option<(usize, usize)>,
    /// 剪枝目标 token 数。
    pub prune_to_tokens: i64,
}

impl CompactPlan {
    /// 是否什么都不需要做。
    pub fn is_noop(&self) -> bool {
        self.prune_tool_results.is_empty() && self.drop.is_empty() && self.summarize.is_none()
    }
}

/// 压缩配置。
#[derive(Debug, Clone)]
pub struct CompactCfg {
    /// 总 token 预算。
    pub budget: i64,
    /// 触发压缩的水位（预算的比例，如 0.8 表示用到 80% 就开始压）。
    pub trigger_ratio: f32,
    /// 工具结果剪枝后的目标 token 数。
    pub prune_tool_to_tokens: i64,
    /// 始终保留的最近消息条数（不剪不丢，保证近期上下文完整）。
    pub keep_recent: usize,
    /// 丢弃仍不够时是否启用摘要。
    pub enable_summary: bool,
}

impl Default for CompactCfg {
    fn default() -> Self {
        Self {
            budget: 8000,
            trigger_ratio: 0.8,
            prune_tool_to_tokens: 200,
            keep_recent: 6,
            enable_summary: false,
        }
    }
}

/// 默认预留 token（输出 + 系统提示 + 压缩本身的余量）。对齐 OpenClaw reserveTokens。
pub const DEFAULT_RESERVE_TOKENS: i64 = 16_384;

/// 预算绝对下限：即使窗口很小，也保证给对话内容留这么多。对齐 OpenClaw MIN_PROMPT_BUDGET。
pub const MIN_BUDGET_TOKENS: i64 = 8_000;

impl CompactCfg {
    /// 从上下文窗口派生预算：`budget = window − reserve`，并保底 [`MIN_BUDGET_TOKENS`]。
    ///
    /// 其余参数取默认（trigger_ratio/prune/keep_recent）。
    pub fn from_window(context_window: i64, reserve: i64) -> Self {
        let budget = (context_window - reserve).max(MIN_BUDGET_TOKENS);
        Self {
            budget,
            ..Default::default()
        }
    }
}

/// 规划一次压缩（纯函数）。
///
/// `metas` 按时间正序。返回的计划保证：
/// - 最近 `keep_recent` 条不被剪枝/丢弃
/// - 优先剪枝旧工具结果，其次丢弃最早消息
pub fn plan_compaction(metas: &[MsgMeta], cfg: &CompactCfg) -> CompactPlan {
    let total: i64 = metas.iter().map(|m| m.tokens).sum();
    let trigger = (cfg.budget as f32 * cfg.trigger_ratio) as i64;

    let mut plan = CompactPlan {
        prune_to_tokens: cfg.prune_tool_to_tokens,
        ..Default::default()
    };

    if total <= trigger {
        return plan; // 未到水位，无需压缩
    }

    // 可动区间：排除最近 keep_recent 条。
    let movable_end = metas.len().saturating_sub(cfg.keep_recent);
    if movable_end == 0 {
        return plan; // 全是近期消息，不动
    }

    let mut projected = total;

    // 第 1 步：剪枝旧工具结果（从最早开始）。
    for m in metas[..movable_end].iter() {
        if projected <= trigger {
            break;
        }
        if m.is_tool_result && m.tokens > cfg.prune_tool_to_tokens {
            plan.prune_tool_results.push(m.index);
            projected -= m.tokens - cfg.prune_tool_to_tokens;
        }
    }

    // 第 2 步：仍超预算 → 丢弃最早消息。
    //
    // 工具对成对进出：丢 dispatch 就连带丢它后面的结果，丢结果就连带丢前面的
    // dispatch。留下半个对子会让 provider 400。
    if projected > trigger {
        // 已剪枝的按剪枝后大小计。
        let effective = |m: &MsgMeta| {
            if plan.prune_tool_results.contains(&m.index) {
                cfg.prune_tool_to_tokens
            } else {
                m.tokens
            }
        };
        for (i, m) in metas[..movable_end].iter().enumerate() {
            if projected <= trigger {
                break;
            }
            if plan.drop.contains(&m.index) {
                continue; // 已作为配对的另一半被丢过
            }

            // 配对的另一半：dispatch 看后一条，结果看前一条。
            let mate_pos = if m.is_tool_dispatch {
                Some(i + 1).filter(|&j| metas.get(j).is_some_and(|n| n.is_tool_result))
            } else if m.is_tool_result {
                i.checked_sub(1).filter(|&j| metas[j].is_tool_dispatch)
            } else {
                None
            };

            // 另一半落在 keep_recent 保护区里 → 整对都不动。keep_recent 是硬承诺，
            // 不能为了腾预算把近期消息拽走；只丢这一半又会留下孤儿。
            if mate_pos.is_some_and(|j| j >= movable_end) {
                continue;
            }

            plan.drop.push(m.index);
            projected -= effective(m);
            if let Some(mate) = mate_pos.map(|j| &metas[j]) {
                if !plan.drop.contains(&mate.index) {
                    plan.drop.push(mate.index);
                    projected -= effective(mate);
                }
            }
        }
        plan.drop.sort_unstable();
    }

    // 第 3 步：若启用摘要，把被丢弃的区间标记为待摘要（保留语义）。
    if cfg.enable_summary && !plan.drop.is_empty() {
        let start = *plan.drop.iter().min().unwrap();
        let end = *plan.drop.iter().max().unwrap() + 1;
        plan.summarize = Some((start, end));
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(index: usize, tokens: i64, is_tool: bool) -> MsgMeta {
        MsgMeta { index, tokens, is_tool_result: is_tool, is_tool_dispatch: false }
    }

    /// `assistant(tool_calls)` → `tool(result)` 相邻一对。
    fn pair(dispatch_index: usize, dispatch_tokens: i64, result_tokens: i64) -> [MsgMeta; 2] {
        [
            MsgMeta {
                index: dispatch_index,
                tokens: dispatch_tokens,
                is_tool_result: false,
                is_tool_dispatch: true,
            },
            meta(dispatch_index + 1, result_tokens, true),
        ]
    }

    #[test]
    fn under_budget_is_noop() {
        let metas = vec![meta(0, 100, false), meta(1, 100, false)];
        let cfg = CompactCfg { budget: 1000, ..Default::default() };
        let plan = plan_compaction(&metas, &cfg);
        assert!(plan.is_noop(), "未到水位不应压缩");
    }

    #[test]
    fn prunes_old_tool_results_first() {
        // 一条超大工具结果在前，后面若干小消息。
        let mut metas = vec![meta(0, 5000, true)];
        for i in 1..=8 {
            metas.push(meta(i, 100, false));
        }
        let cfg = CompactCfg {
            budget: 2000,
            trigger_ratio: 0.8,
            prune_tool_to_tokens: 200,
            keep_recent: 3,
            enable_summary: false,
        };
        let plan = plan_compaction(&metas, &cfg);
        assert!(plan.prune_tool_results.contains(&0), "应剪枝大工具结果");
        // 剪枝后已足够，不必丢弃。
        assert!(plan.drop.is_empty(), "剪枝够了就不该丢弃: {plan:?}");
    }

    #[test]
    fn drops_earliest_when_pruning_insufficient() {
        // 全是普通消息（无工具结果可剪），只能丢最早的。
        let metas: Vec<MsgMeta> = (0..10).map(|i| meta(i, 500, false)).collect();
        let cfg = CompactCfg {
            budget: 2000,
            trigger_ratio: 0.8, // 触发线 1600
            prune_tool_to_tokens: 200,
            keep_recent: 2,
            enable_summary: false,
        };
        let plan = plan_compaction(&metas, &cfg);
        assert!(plan.prune_tool_results.is_empty(), "无工具结果可剪");
        assert!(!plan.drop.is_empty(), "应丢弃最早消息");
        // 丢弃应从最早开始。
        assert_eq!(plan.drop[0], 0);
        // 最近 2 条不能被丢。
        assert!(!plan.drop.contains(&8) && !plan.drop.contains(&9), "近期消息不应被丢");
    }

    #[test]
    fn keeps_recent_messages_untouched() {
        let metas: Vec<MsgMeta> = (0..5).map(|i| meta(i, 10_000, true)).collect();
        let cfg = CompactCfg {
            budget: 1000,
            keep_recent: 5, // 全部都是"近期"
            ..Default::default()
        };
        let plan = plan_compaction(&metas, &cfg);
        assert!(plan.is_noop(), "全为近期消息时不应动");
    }

    #[test]
    fn from_window_derives_budget() {
        // 大窗口：budget = window − reserve。
        let c = CompactCfg::from_window(65_536, DEFAULT_RESERVE_TOKENS);
        assert_eq!(c.budget, 65_536 - 16_384);
        // 小窗口：保底不低于 MIN_BUDGET_TOKENS。
        let c = CompactCfg::from_window(10_000, DEFAULT_RESERVE_TOKENS);
        assert_eq!(c.budget, MIN_BUDGET_TOKENS);
    }

    /// 工具对成对进出：丢了 dispatch 必须连带丢它的结果，反之亦然。
    /// 留下半个对子（孤儿 tool，或指向不存在结果的 tool_calls）会让 provider 400。
    #[test]
    fn drops_tool_pairs_together() {
        // [0]=usr, [1..2]=工具对, [3..4]=工具对, [5..9]=普通消息
        let mut metas = vec![meta(0, 500, false)];
        metas.extend(pair(1, 300, 4000));
        metas.extend(pair(3, 300, 4000));
        for i in 5..10 {
            metas.push(meta(i, 500, false));
        }
        let cfg = CompactCfg {
            budget: 2000,
            trigger_ratio: 0.8,
            prune_tool_to_tokens: 200,
            keep_recent: 2,
            enable_summary: false,
        };
        let plan = plan_compaction(&metas, &cfg);

        for (dispatch, result) in [(1usize, 2usize), (3, 4)] {
            assert_eq!(
                plan.drop.contains(&dispatch),
                plan.drop.contains(&result),
                "工具对 ({dispatch},{result}) 必须成对进出，实际 drop={:?}",
                plan.drop
            );
        }
    }

    /// 结果落在 keep_recent 保护区时，整对都不丢——只丢 dispatch 会留下孤儿，
    /// 而把结果一起拽走就破了 keep_recent 的硬承诺。
    #[test]
    fn never_orphans_result_inside_keep_recent() {
        // [0..3]=普通消息, [4]=dispatch, [5]=结果（落在 keep_recent=2 里）
        let mut metas: Vec<MsgMeta> = (0..4).map(|i| meta(i, 3000, false)).collect();
        metas.extend(pair(4, 300, 3000));
        let cfg = CompactCfg {
            budget: 1000,
            trigger_ratio: 0.8,
            prune_tool_to_tokens: 200,
            keep_recent: 2,
            enable_summary: false,
        };
        let plan = plan_compaction(&metas, &cfg);
        assert!(!plan.drop.contains(&4), "dispatch 的结果在保护区，整对都不该动: {plan:?}");
        assert!(!plan.drop.contains(&5), "keep_recent 内的消息不该被丢: {plan:?}");
        assert!(!plan.drop.is_empty(), "前面的普通消息仍应被丢以腾预算");
    }

    #[test]
    fn summary_marks_dropped_range() {
        let metas: Vec<MsgMeta> = (0..10).map(|i| meta(i, 500, false)).collect();
        let cfg = CompactCfg {
            budget: 2000,
            trigger_ratio: 0.8,
            prune_tool_to_tokens: 200,
            keep_recent: 2,
            enable_summary: true,
        };
        let plan = plan_compaction(&metas, &cfg);
        let (start, end) = plan.summarize.expect("应标记摘要区间");
        assert_eq!(start, 0);
        assert!(end <= 8, "摘要区间不应覆盖近期消息");
    }
}
