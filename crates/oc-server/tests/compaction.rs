//! M5 第 3 段：验证长对话触发上下文压缩，到达模型的消息被裁剪且不崩，
//! 最近消息保留。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;
use oc_server::testing::{test_cfg, SessionConfigExt};

fn cfg(budget: i64) -> SessionConfig {
    test_cfg().with_soul("人格").with_history(500, budget)
}

async fn wait_terminal(rx: &mut broadcast::Receiver<Event>, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(
            ev,
            Event::Lifecycle { phase: LifecyclePhase::End, .. }
                | Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
        ) {
            return;
        }
    }
}

#[tokio::test]
async fn long_history_gets_compacted_before_model() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    // 预置大量历史：40 条，每条约 100 token（400 字符）。
    let big = "字".repeat(400);
    for i in 0..40 {
        let role = if i % 2 == 0 {
            oc_store::Role::User
        } else {
            oc_store::Role::Assistant
        };
        w.append_entry(oc_store::NewEntry {
            session_id: "main".into(),
            role,
            content: format!("{big}#{i}"),
            tokens_est: 100,
        })
        .await
        .unwrap();
    }

    // 小预算触发压缩。
    let (tx, mut rx) = broadcast::channel(512);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    let handle = session::spawn(oc_proto::SessionId::main(), cfg(1000), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

    handle.submit("最新一句".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    assert_eq!(reqs.len(), 1, "应发起一次模型请求");
    let sent = &reqs[0].messages;

    // 到达模型的消息应远少于总历史（41 条），说明压缩生效。
    assert!(
        sent.len() < 41,
        "压缩后消息数应减少，实际 {} 条",
        sent.len()
    );
    // 最近消息应保留：最新用户输入必须在。
    assert!(
        sent.iter().any(|m| m.content == "最新一句"),
        "最近消息必须保留"
    );
}

#[tokio::test]
async fn short_history_not_compacted() {
    let store = oc_store::Store::open_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    // 大预算，短对话，不应压缩。
    let handle = session::spawn(oc_proto::SessionId::main(), cfg(100_000), provider, tx, store, oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

    handle.submit("就一句".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    assert_eq!(reqs[0].messages.len(), 1, "短对话不应被压缩");
    assert_eq!(reqs[0].messages[0].content, "就一句");
}
