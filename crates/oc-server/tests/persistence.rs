//! M5 对话持久化测试：落库 + 重启后加载历史（"重启不失忆"）。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::MockProvider;
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
        soul: String::new(),
        skills: Vec::new(),
        trigger_threshold: 0.72,
        trigger_max_per_turn: 3,
        intent_defaults: Default::default(),
        soul_dir: None,
        context_window: 65536,
    }
}

/// 等到 run 结束（End 或 Error）。
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
async fn conversation_persists_and_reloads() {
    // 共享同一个内存库，模拟"同一个数据库、两次 daemon 生命周期"。
    let store = oc_store::Store::open_memory().expect("store");

    // ── 第一次"启动"：说一句话，等回复落库 ──
    {
        let (tx, mut rx) = broadcast::channel(256);
        let provider = Arc::new(MockProvider::echo_text("我记住了"));
        let handle = session::spawn(oc_proto::SessionId::main(), cfg(), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

        handle.submit("我叫郭睿".into(), handle.broadcast_sink()).await.expect("run");
        wait_terminal(&mut rx, Duration::from_secs(5)).await;
    }

    // 落库应有 user + assistant 两条。
    let hist = store
        .writer()
        .load_transcript("main".into(), 100)
        .await
        .expect("load");
    assert_eq!(hist.len(), 2, "应落库 user + assistant，实际 {:?}", hist.len());
    assert_eq!(hist[0].content, "我叫郭睿");
    assert_eq!(hist[0].role, oc_store::Role::User);
    assert_eq!(hist[1].content, "我记住了");
    assert_eq!(hist[1].role, oc_store::Role::Assistant);

    // ── 第二次"启动"（新 session actor，同一个库）：历史应被加载 ──
    {
        let (tx, mut rx) = broadcast::channel(256);
        let provider = Arc::new(MockProvider::echo_text("你叫郭睿"));
        let handle = session::spawn(oc_proto::SessionId::main(), cfg(), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

        handle.submit("我叫什么".into(), handle.broadcast_sink()).await.expect("run");
        wait_terminal(&mut rx, Duration::from_secs(5)).await;
    }

    // 现在应有 4 条（两轮对话），说明历史累积而非清空。
    let hist2 = store
        .writer()
        .load_transcript("main".into(), 100)
        .await
        .expect("load");
    assert_eq!(hist2.len(), 4, "跨'重启'历史应累积");
    assert_eq!(hist2[2].content, "我叫什么");
    assert_eq!(hist2[3].content, "你叫郭睿");
}

#[tokio::test]
async fn reset_clears_context_but_keeps_transcript() {
    let store = oc_store::Store::open_memory().expect("store");
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(MockProvider::echo_text("好"));
    let handle = session::spawn(oc_proto::SessionId::main(), cfg(), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));
    handle.submit("第一句".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    assert_eq!(w.load_transcript("main".into(), 100).await.unwrap().len(), 2);

    // reset：上下文起点前移，load 变空。
    w.reset_session("main".into()).await.unwrap();
    assert!(
        w.load_transcript("main".into(), 100).await.unwrap().is_empty(),
        "reset 后活跃上下文应为空"
    );

    // reset 后新一轮仍能正常对话（起点之后重新累积）。
    let (tx2, mut rx2) = broadcast::channel(256);
    let provider2 = Arc::new(MockProvider::echo_text("新的开始"));
    let handle2 = session::spawn(oc_proto::SessionId::main(), cfg(), provider2, tx2, store.clone(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));
    handle2.submit("重新开始".into(), handle2.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx2, Duration::from_secs(5)).await;

    let after = w.load_transcript("main".into(), 100).await.unwrap();
    assert_eq!(after.len(), 2, "reset 后应只看到新一轮");
    assert_eq!(after[0].content, "重新开始");

    // 注：entry 行本身未被删除（reset 只推进 reset_at），
    // 该行为由 oc-store 的 ops::reset_session 单测覆盖。
}
