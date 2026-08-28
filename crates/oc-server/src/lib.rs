//! oc 常驻进程（设计 §7）。
//!
//! M2 落地：本地 socket / 命名管道服务端、NDJSON 帧、req/res/event、
//! 事件广播 (broadcast)、幂等缓存。`chat.send` 暂以 **echo** 演示事件流
//! （agent loop 在 M3）。

pub mod codec;
pub mod conn;
pub mod dispatch;
pub mod error;
pub mod state;
pub mod transport;

pub use error::{ServerError, ServerResult};
pub use state::ServerState;
pub use transport::TransportKind;

use std::sync::Arc;

use tokio::sync::broadcast;
use tracing::info;

/// 事件广播容量（每 client 独立订阅；慢 client 丢事件后重拉快照）。
const EVENT_CHANNEL_CAP: usize = 256;

/// 启动服务端，监听给定传输，直到收到关停信号。
pub async fn serve(kind: TransportKind) -> ServerResult<()> {
    let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);
    let state = Arc::new(ServerState::new(event_tx));

    let mut listener = transport::Listener::bind(&kind)?;
    info!(?kind, "oc-server 监听中");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = accepted?;
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = conn::handle(stream, state).await {
                        tracing::warn!(error = %e, "连接处理结束");
                    }
                });
            }
            _ = shutdown_signal() => {
                info!("收到关停信号，退出");
                break;
            }
        }
    }
    Ok(())
}

/// 等待 Ctrl-C（跨平台）。
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
