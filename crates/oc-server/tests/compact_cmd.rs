//! /compact 手动压缩：摘要旧历史 → 落库（reset_at 推进 + 摘要 entry）。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_proto::SessionId;
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;
use oc_server::testing::test_cfg;

fn cfg() -> SessionConfig {
    test_cfg()
}

#[tokio::test]
async fn compact_summarizes_old_history() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();
    // 预置 10 条历史（足够超过 keep_recent）。
    for i in 0..10 {
        let role = if i % 2 == 0 { oc_store::Role::User } else { oc_store::Role::Assistant };
        w.append_entry(oc_store::NewEntry::text(
            "main",
            role,
            format!("历史消息 {i}"),
            5,
        ))
        .await
        .unwrap();
    }

    // mock 摘要返回固定文本。
    let provider = Arc::new(CapturingMock::new("这是压缩后的结构化摘要文本"));
    let (tx, _rx) = broadcast::channel(64);
    let handle = session::spawn(SessionId::main(), cfg(), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

    handle.compact().await;

    // compact 是异步执行，轮询等落库生效（摘要 entry 出现）。
    let mut ok = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let hist = w.load_transcript("main".into(), 100).await.unwrap();
        if hist.iter().any(|e| e.content.contains("上下文摘要")) {
            // 应含摘要 + 最近若干条，且旧消息被排除。
            assert!(hist.len() < 10, "压缩后条数应减少: {}", hist.len());
            assert!(hist.iter().any(|e| e.content.contains("结构化摘要文本")), "应含摘要文本");
            assert!(!hist.iter().any(|e| e.content.contains("历史消息 0")), "最早消息应被排除");
            ok = true;
            break;
        }
    }
    assert!(ok, "compact 应在超时内完成落库");
}

#[tokio::test]
async fn compact_short_history_notifies_instead_of_silent() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();
    // 只放 2 条（不足 keep_recent+1），应跳过压缩但给反馈。
    for (role, text) in [(oc_store::Role::User, "hi"), (oc_store::Role::Assistant, "在")] {
        w.append_entry(oc_store::NewEntry::text("main", role, text, 1))
            .await
            .unwrap();
    }

    let provider = Arc::new(CapturingMock::new("不应被调用"));
    let (tx, mut rx) = broadcast::channel(64);
    let handle = session::spawn(SessionId::main(), cfg(), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

    handle.compact().await;

    // 应收到一条 Proactive 通知，说明无需压缩（而非静默）。
    let mut notified = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let oc_proto::Event::Proactive { text, .. } = ev {
            assert!(text.contains("无需压缩"), "应提示无需压缩: {text}");
            notified = true;
            break;
        }
    }
    assert!(notified, "历史过短时应给出明确反馈，而非静默跳过");
}
