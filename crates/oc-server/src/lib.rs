//! oc 常驻进程（设计 §7）。
//!
//! M3 落地：agent 循环（经 oc-core 状态机 + oc-llm 流）、空闲看门狗 /
//! run 超时 / abort、心跳 tick 底座。`chat.send` 走真正的模型轮。

pub mod codec;
pub mod conn;
pub mod dispatch;
pub mod dreaming;
pub mod error;
pub mod ledger;
pub mod proactive;
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

/// 每多少个心跳 tick 触发一轮 dreaming 巩固（稀疏，避免频繁重写）。
const DREAM_EVERY_TICKS: u64 = 60;

/// 启动服务端。
///
/// `provider`：模型 provider（真实或 mock）。`session_cfg`：会话/防卡死参数。
pub async fn serve_with(
    kind: TransportKind,
    provider: Arc<dyn Provider>,
    session_cfg: SessionConfig,
    heartbeat_interval: Duration,
    store: oc_store::Store,
) -> ServerResult<()> {
    let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);

    // 审批注册表：EventApprovalHandler 与 ServerState 共享。
    let approvals: state::ApprovalRegistry = Arc::new(dashmap::DashMap::new());

    // 后台任务台账。
    let ledger = ledger::TaskLedger::new(event_tx.clone());

    // 若配置了工具，注入交互式审批处理器 + 把后台移交接到台账。
    let mut session_cfg = session_cfg;
    if let Some(tools) = session_cfg.tools.take() {
        let handler = Arc::new(tools_bridge::EventApprovalHandler::new(
            event_tx.clone(),
            Arc::clone(&approvals),
        ));
        // 后台移交 → 台账登记。
        if let Some(mut rx) = tools.take_handoff() {
            let ledger2 = ledger.clone();
            tokio::spawn(async move {
                while let Some(handoff) = rx.recv().await {
                    ledger2.register(handoff);
                }
            });
        }
        session_cfg.tools = Some(tools.with_approval(handler));
    }

    // proactive 上下文：在 provider 被 session 接管前克隆出所需句柄。
    let proactive_ctx = proactive::ProactiveCtx {
        provider: Arc::clone(&provider),
        events: event_tx.clone(),
        store: store.clone(),
        model: session_cfg.model.clone(),
        soul: session_cfg.soul.clone(),
    };

    let session = session::spawn(session_cfg, provider, event_tx.clone(), store.clone());
    let dream_store = store.clone();
    let state = Arc::new(ServerState::new(event_tx, session, approvals, ledger, store));

    // 心跳 tick：每 tick 卡死诊断扫描 + cron 到期扫描；每 DREAM_EVERY_TICKS 一轮 dreaming。
    let shutdown = CancellationToken::new();
    let scan_session = state.session().clone();
    scheduler::Heartbeat::new(heartbeat_interval).spawn(shutdown.clone(), move |tick| {
        let session = scan_session.clone();
        let store = dream_store.clone();
        let pctx = proactive_ctx.clone();
        async move {
            tracing::debug!(tick, "heartbeat：扫描");
            session.health_scan().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // cron 到期触发（失败不阻塞）。
            proactive::cron_scan(&pctx, now).await;
            // dreaming 巩固：稀疏触发（设计 §7.4 夜间/空闲；M5 先按 tick 周期）。
            if tick % DREAM_EVERY_TICKS == 0 {
                dreaming::scan(&store, now, &oc_core::dreaming::DreamCfg::default()).await;
            }
        }
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
