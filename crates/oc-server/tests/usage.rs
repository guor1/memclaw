//! 上下文用量：provider 报告 usage → 广播 Event::Usage（供 client 显示已用/窗口）。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::types::{Delta, FinishReason, Usage};
use oc_proto::{Event, SessionId};
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;

fn cfg(window: u32) -> SessionConfig {
    SessionConfig {
        model: "mock".into(),
        system_prompt: None,
        idle_timeout: Duration::from_secs(5),
        run_timeout: None,
        queue_cap: 8,
        tools: None,
        warn_secs: 60,
        abort_min_secs: 300,
        max_history_entries: 200,
        history_token_budget: 8000,
        soul: String::new(),
        skills: Vec::new(),
        trigger_threshold: 0.72,
        trigger_max_per_turn: 3,
        intent_defaults: Default::default(),
        soul_dir: None,
        default_tz: "UTC".into(),
        context_window: window,
    }
}

#[tokio::test]
async fn usage_delta_emits_usage_event() {
    // 脚本：文本 + 真实 usage（input=1000）+ 结束。
    let script = vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text("好的".into()) },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Usage(Usage { input_tokens: 1000, output_tokens: 20 }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ];
    let provider = Arc::new(MockProvider::scripted(script));
    let (tx, mut rx) = broadcast::channel(256);
    let handle = session::spawn(SessionId::main(), cfg(64_000), provider, tx, oc_store::Store::open_memory().unwrap(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

    handle.submit("你好".into(), handle.broadcast_sink()).await.expect("run");

    // 收集事件，找 Usage。
    let mut got: Option<(u32, u32)> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let Event::Usage { input_tokens, context_window, session } = ev {
            assert_eq!(session, SessionId::main());
            got = Some((input_tokens, context_window));
            break;
        }
    }
    let (used, window) = got.expect("应收到 Usage 事件");
    assert_eq!(used, 1000, "已用 token 应来自 provider usage");
    assert_eq!(window, 64_000, "窗口应为配置值");
}
