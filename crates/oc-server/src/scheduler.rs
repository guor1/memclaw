//! 心跳 tick 底座（设计 §7.4，M3 最小版）。
//!
//! M3：固定间隔 `tokio::time::interval`，仅按时触发回调（不含 cron/anti-nagging）。
//! 供 M4 卡死扫描 / M5 dreaming 挂载。M6 接入 cron 最小堆。

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::trace;

/// 心跳 tick 计数器（可观测：验收要求"tick 按间隔可观测"）。
pub struct Heartbeat {
    interval: Duration,
}

impl Heartbeat {
    pub fn new(interval: Duration) -> Self {
        Self { interval }
    }

    /// 启动心跳循环，每个 tick 调用 `on_tick(tick_no)`。
    /// 通过 `shutdown` 取消。
    pub fn spawn<F, Fut>(self, shutdown: CancellationToken, mut on_tick: F)
    where
        F: FnMut(u64) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.interval);
            let mut tick_no: u64 = 0;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        tick_no += 1;
                        trace!(tick = tick_no, "heartbeat tick");
                        on_tick(tick_no).await;
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
        });
    }
}
