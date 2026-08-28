//! oc 常驻进程（设计 §7）。
//!
//! M3 落地：agent 循环（经 oc-core 状态机 + oc-llm 流）、空闲看门狗 /
//! run 超时 / abort、心跳 tick 底座。`chat.send` 走真正的模型轮。

pub mod codec;
pub mod conn;
pub mod dispatch;
pub mod error;
pub mod run;
pub mod scheduler;
pub mod session;
pub mod state;
pub mod tools_bridge;
pub mod transport;

pub use error::{ServerError, ServerResult};
pub use session::SessionConfig;
pub use state::ServerState;
pub use transport::TransportKind;

use std::sync::Arc;
use std::time::Duration;

use oc_llm::Provider;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

const EVENT_CHANNEL_CAP: usize = 256;

/// 启动服务端。
///
/// `provider`：模型 provider（真实或 mock）。`session_cfg`：会话/防卡死参数。
pub async fn serve_with(
    kind: TransportKind,
    provider: Arc<dyn Provider>,
    session_cfg: SessionConfig,
    heartbeat_interval: Duration,
) -> ServerResult<()> {
    let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);

    // 审批注册表：EventApprovalHandler 与 ServerState 共享。
    let approvals: state::ApprovalRegistry = Arc::new(dashmap::DashMap::new());

    // 若配置了工具，注入交互式审批处理器。
    let mut session_cfg = session_cfg;
    if let Some(tools) = session_cfg.tools.take() {
        let handler = Arc::new(tools_bridge::EventApprovalHandler::new(
            event_tx.clone(),
            Arc::clone(&approvals),
        ));
        session_cfg.tools = Some(tools.with_approval(handler));
    }

    let session = session::spawn(session_cfg, provider, event_tx.clone());
    let state = Arc::new(ServerState::new(event_tx, session, approvals));

    // 心跳 tick 底座（M3：占位回调；M4/M5 挂扫描/dreaming）。
    let shutdown = CancellationToken::new();
    scheduler::Heartbeat::new(heartbeat_interval).spawn(shutdown.clone(), move |tick| async move {
        tracing::debug!(tick, "heartbeat（M3 占位）");
    });

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
                shutdown.cancel();
                break;
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
