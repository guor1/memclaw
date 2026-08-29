//! M5 第 4 段：Lane1 记忆注入端到端。预置 curated 记忆，发相关消息，
//! 验证记忆作为 bootstrap 出现在系统提示词里（"记得住你"）。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;

fn cfg() -> SessionConfig {
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
        soul: "人格".into(),
        skills: Vec::new(),
        trigger_threshold: 0.5,
        trigger_max_per_turn: 3,
    }
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
async fn curated_memory_injected_on_relevant_message() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    // 预置一条 curated 记忆（用户偏好）。
    w.upsert_memory(oc_store::NewMemory {
        id: "pref-1".into(),
        tier: oc_store::Tier::Curated,
        origin: oc_store::Origin::Owner,
        text: "用户喜欢简洁直接的回复".into(),
        keywords: Some("回复 简洁".into()),
        importance: 0.9,
        content_hash: "h".into(),
    })
    .await
    .unwrap();

    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好的"));
    let captures = provider.captures();
    let handle = session::spawn(cfg(), provider, tx, store.clone());

    // 消息里含记忆的关键词"回复"。
    handle.submit("你平时怎么组织回复的".into()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    let system = reqs[0].system.as_deref().unwrap_or("");
    assert!(
        system.contains("用户喜欢简洁直接的回复"),
        "相关 curated 记忆应注入系统提示词: {system}"
    );
}

#[tokio::test]
async fn irrelevant_message_does_not_inject() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();
    w.upsert_memory(oc_store::NewMemory {
        id: "pref-1".into(),
        tier: oc_store::Tier::Curated,
        origin: oc_store::Origin::Owner,
        text: "用户喜欢登山".into(),
        keywords: Some("登山".into()),
        importance: 0.9,
        content_hash: "h".into(),
    })
    .await
    .unwrap();

    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    let handle = session::spawn(cfg(), provider, tx, store.clone());

    handle.submit("帮我写段代码".into()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    let system = reqs[0].system.as_deref().unwrap_or("");
    assert!(
        !system.contains("用户喜欢登山"),
        "不相关消息不应注入该记忆: {system}"
    );
}
