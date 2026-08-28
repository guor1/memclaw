//! 队列 + steer/interrupt + 卡死诊断（设计 §4.3）。
//!
//! M3：主会话串行车道 + `chat.abort`（仅中止活跃 run）+ 卡死诊断纯判定
//! （供 M4 心跳扫描）。完整 steer/collect/drain 语义在 M4。

use std::collections::VecDeque;

/// 排队的一轮用户输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedTurn {
    pub run_id: String,
    pub text: String,
}

/// 串行运行队列。同一时刻至多一个活跃 run。
#[derive(Debug, Default)]
pub struct RunQueue {
    active: Option<String>, // 活跃 run_id
    pending: VecDeque<QueuedTurn>,
    cap: usize,
}

impl RunQueue {
    pub fn new(cap: usize) -> Self {
        Self {
            active: None,
            pending: VecDeque::new(),
            cap,
        }
    }

    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    pub fn is_idle(&self) -> bool {
        self.active.is_none()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 尝试入队；若无活跃则直接成为活跃并返回 `Started`。
    pub fn submit(&mut self, turn: QueuedTurn) -> SubmitResult {
        if self.active.is_none() {
            self.active = Some(turn.run_id.clone());
            SubmitResult::Started(turn)
        } else if self.pending.len() >= self.cap {
            SubmitResult::Rejected
        } else {
            self.pending.push_back(turn);
            SubmitResult::Queued
        }
    }

    /// 当前活跃 run 结束，取下一个（若有）设为活跃。
    pub fn complete_active(&mut self) -> Option<QueuedTurn> {
        self.active = None;
        if let Some(next) = self.pending.pop_front() {
            self.active = Some(next.run_id.clone());
            Some(next)
        } else {
            None
        }
    }

    /// 软中止：清空排队轮（M4 `/stop` 的前半）。返回被丢弃的数量。
    pub fn drain_pending(&mut self) -> usize {
        let n = self.pending.len();
        self.pending.clear();
        n
    }
}

#[derive(Debug, PartialEq)]
pub enum SubmitResult {
    /// 立即成为活跃 run。
    Started(QueuedTurn),
    /// 入队等待。
    Queued,
    /// 队列已满，拒绝。
    Rejected,
}

/// 卡死诊断（设计 §10.2 / §6）。纯判定，供心跳扫描调用。
///
/// `warn_secs` 为警告阈值；abort 需同时满足 `elapsed ≥ abort_min_secs`
/// 且 `elapsed ≥ 3 × warn_secs`（慢 ≠ 卡）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunHealth {
    Healthy,
    /// 慢但未卡（超警告阈值，未达 abort 条件）。
    LongRunning,
    /// 达到 abort 条件，应释放车道。
    Stuck,
}

pub fn diagnose(elapsed_secs: u64, warn_secs: u64, abort_min_secs: u64) -> RunHealth {
    if elapsed_secs < warn_secs {
        RunHealth::Healthy
    } else if elapsed_secs >= abort_min_secs && elapsed_secs >= warn_secs.saturating_mul(3) {
        RunHealth::Stuck
    } else {
        RunHealth::LongRunning
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str) -> QueuedTurn {
        QueuedTurn { run_id: id.into(), text: "x".into() }
    }

    #[test]
    fn first_submit_starts() {
        let mut q = RunQueue::new(4);
        assert!(matches!(q.submit(turn("a")), SubmitResult::Started(_)));
        assert_eq!(q.active(), Some("a"));
    }

    #[test]
    fn second_submit_queues_then_promotes() {
        let mut q = RunQueue::new(4);
        q.submit(turn("a"));
        assert_eq!(q.submit(turn("b")), SubmitResult::Queued);
        let next = q.complete_active().unwrap();
        assert_eq!(next.run_id, "b");
        assert_eq!(q.active(), Some("b"));
    }

    #[test]
    fn rejects_when_full() {
        let mut q = RunQueue::new(1);
        q.submit(turn("a"));
        q.submit(turn("b")); // fills pending
        assert_eq!(q.submit(turn("c")), SubmitResult::Rejected);
    }

    #[test]
    fn drain_clears_pending() {
        let mut q = RunQueue::new(4);
        q.submit(turn("a"));
        q.submit(turn("b"));
        q.submit(turn("c"));
        assert_eq!(q.drain_pending(), 2);
        assert_eq!(q.pending_len(), 0);
    }

    #[test]
    fn diagnose_thresholds() {
        // warn=60, abort_min=300
        assert_eq!(diagnose(30, 60, 300), RunHealth::Healthy);
        assert_eq!(diagnose(120, 60, 300), RunHealth::LongRunning); // 超警告但未达 abort
        assert_eq!(diagnose(200, 60, 300), RunHealth::LongRunning); // ≥3×warn 但 <abort_min
        assert_eq!(diagnose(400, 60, 300), RunHealth::Stuck); // 同时满足
    }

    #[test]
    fn slow_is_not_stuck() {
        // 慢 run：warn=100, abort_min=300, elapsed=280 → 未达 abort_min
        assert_eq!(diagnose(280, 100, 300), RunHealth::LongRunning);
    }
}
