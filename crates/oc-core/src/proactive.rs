//! 主动性纯策略（设计 §4.7、§12）。
//!
//! **纯函数集合**：anti-nagging 判定（cooldown/budget/expiry）、到期筛选、cron 下次触发。
//! tokio timer / spawn / 起子会话全在 server；本模块只出判定与时间计算。
//!
//! cron 用自带 5 字段解析器（省外部依赖）。**表达式按给定 IANA 时区解释**
//! （P1-5 修复；此前一律按 UTC，`tz` 字段存了没人读）。一次性延时任务用
//! [`ONCE_EXPR`] 标记，`next_at` 直接存绝对秒、不参与表达式推算。

mod cron;

pub use cron::{fmt_in_tz, is_once, next_fire, CronParseError, ONCE_EXPR};

/// anti-nagging 配置（设计 §12.5 默认：cooldown 24h / budget 3 / expiry 90d）。
#[derive(Debug, Clone)]
pub struct NagCfg {
    /// 两次触发的最小间隔（秒）。
    pub cooldown_secs: i64,
    /// 触发次数上限（用尽即静默）。
    pub budget: u32,
    /// 过期时长（秒）：距创建超过则不再触发。0 表示不过期。
    pub expiry_secs: i64,
}

impl Default for NagCfg {
    fn default() -> Self {
        Self {
            cooldown_secs: 24 * 3600,
            budget: 3,
            expiry_secs: 90 * 86400,
        }
    }
}

/// 一条待触发项的运行时状态（从 store 取回映射而来）。
#[derive(Debug, Clone)]
pub struct IntentState {
    /// 创建时间（unix 秒），用于过期判定。
    pub created_secs: i64,
    /// 最近触发时间（unix 秒）；None = 从未触发。
    pub last_fired_secs: Option<i64>,
    /// 已触发次数。
    pub fired_count: u32,
}

/// 触发判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireDecision {
    /// 允许现在触发。
    Allow,
    /// 拒绝，并给出原因（便于审计/可观测）。
    Deny(DenyReason),
}

/// 拒绝原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// cooldown 未过。
    Cooldown,
    /// budget 用尽。
    BudgetExhausted,
    /// 已过期。
    Expired,
}

/// anti-nagging 判定（设计 §4.7）：是否允许现在触发。
///
/// 判定顺序：过期 → budget → cooldown。任一不过即拒绝并给原因。
pub fn allow_fire(state: &IntentState, now_secs: i64, cfg: &NagCfg) -> FireDecision {
    // 过期：距创建超过 expiry。
    if cfg.expiry_secs > 0 && now_secs - state.created_secs > cfg.expiry_secs {
        return FireDecision::Deny(DenyReason::Expired);
    }
    // budget 用尽。
    if state.fired_count >= cfg.budget {
        return FireDecision::Deny(DenyReason::BudgetExhausted);
    }
    // cooldown 未过。
    if let Some(last) = state.last_fired_secs {
        if now_secs - last < cfg.cooldown_secs {
            return FireDecision::Deny(DenyReason::Cooldown);
        }
    }
    FireDecision::Allow
}

/// 到期筛选（设计 §4.7）：返回 `next_at ≤ now` 的项 id。
///
/// 纯函数：调用方传入 (id, next_at) 列表；None 的 next_at 视为未排期（不到期）。
pub fn eval_due<Id: Clone>(entries: &[(Id, Option<i64>)], now_secs: i64) -> Vec<Id> {
    entries
        .iter()
        .filter_map(|(id, next_at)| match next_at {
            Some(t) if *t <= now_secs => Some(id.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(created: i64, last: Option<i64>, count: u32) -> IntentState {
        IntentState { created_secs: created, last_fired_secs: last, fired_count: count }
    }

    #[test]
    fn allow_when_fresh() {
        let cfg = NagCfg::default();
        // 刚创建、从未触发 → 允许。
        assert_eq!(allow_fire(&state(0, None, 0), 100, &cfg), FireDecision::Allow);
    }

    #[test]
    fn deny_within_cooldown() {
        let cfg = NagCfg::default();
        let now = 100_000;
        // 上次触发在 cooldown 内。
        let s = state(0, Some(now - 3600), 1); // 1h 前，cooldown 24h
        assert_eq!(allow_fire(&s, now, &cfg), FireDecision::Deny(DenyReason::Cooldown));
        // cooldown 已过 → 允许。
        let s2 = state(0, Some(now - 25 * 3600), 1);
        assert_eq!(allow_fire(&s2, now, &cfg), FireDecision::Allow);
    }

    #[test]
    fn deny_when_budget_exhausted() {
        let cfg = NagCfg::default();
        let s = state(0, Some(0), 3); // fired 3 次 = budget
        assert_eq!(
            allow_fire(&s, 100 * 86400, &cfg),
            FireDecision::Deny(DenyReason::Expired) // 先过期
        );
        // 未过期但 budget 用尽。
        let s2 = state(0, Some(0), 3);
        let cfg2 = NagCfg { expiry_secs: 0, ..NagCfg::default() };
        assert_eq!(allow_fire(&s2, 1000, &cfg2), FireDecision::Deny(DenyReason::BudgetExhausted));
    }

    #[test]
    fn deny_when_expired() {
        let cfg = NagCfg::default();
        let s = state(0, None, 0);
        // 91 天后 → 过期。
        assert_eq!(allow_fire(&s, 91 * 86400, &cfg), FireDecision::Deny(DenyReason::Expired));
        // expiry=0 表示永不过期。
        let cfg2 = NagCfg { expiry_secs: 0, ..NagCfg::default() };
        assert_eq!(allow_fire(&s, 9999 * 86400, &cfg2), FireDecision::Allow);
    }

    #[test]
    fn eval_due_filters_past() {
        let entries = vec![
            ("a", Some(50)),
            ("b", Some(150)),
            ("c", None),
            ("d", Some(100)),
        ];
        let due = eval_due(&entries, 100);
        assert_eq!(due, vec!["a", "d"]); // ≤100 且非 None
    }
}
