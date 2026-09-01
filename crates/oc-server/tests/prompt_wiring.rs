//! M5 第 3 段：验证 oc-core::prompt 的组装结果真的到达模型请求。
//!
//! 补 M3 欠账（render_system_prompt 之前未被调用）的回归测试。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;

fn cfg(soul: &str) -> SessionConfig {
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
        soul: soul.to_string(),
        skills: Vec::new(),
        trigger_threshold: 0.72,
        trigger_max_per_turn: 3,
        intent_defaults: Default::default(),
        context_window: 65536,
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
async fn custom_soul_reaches_model_request() {
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    let store = oc_store::Store::open_memory().unwrap();

    let handle = session::spawn(oc_proto::SessionId::main(), cfg("我是测试人格 ZZZ。"), provider, tx, store, oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));
    handle.submit("你好".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    assert_eq!(reqs.len(), 1, "应发起一次模型请求");
    let system = reqs[0].system.as_deref().unwrap_or("");

    // 人格文本应出现在系统提示词里（说明 prompt 组装被真正调用）。
    assert!(system.contains("我是测试人格 ZZZ"), "system 应含 SOUL 文本: {system}");
    // 组装结构应含分节标题与易变时间尾部。
    assert!(system.contains("# 人格"), "应有人格分节");
    assert!(system.contains("# 当前时间"), "时间应在易变尾部");
}

#[tokio::test]
async fn empty_soul_falls_back_to_default_persona() {
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    let store = oc_store::Store::open_memory().unwrap();

    let handle = session::spawn(oc_proto::SessionId::main(), cfg(""), provider, tx, store, oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));
    handle.submit("你好".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    let system = reqs[0].system.as_deref().unwrap_or("");
    assert!(system.contains("oc"), "缺 SOUL.md 应回退内置人格: {system}");
}

#[tokio::test]
async fn skills_reach_model_request() {
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    let store = oc_store::Store::open_memory().unwrap();

    let mut c = cfg("人格");
    c.skills = vec![oc_core::prompt::SkillBrief {
        name: "pdf".into(),
        body: "生成 PDF 时用 XXXPDFSKILL 工具链。".into(),
    }];

    let handle = session::spawn(oc_proto::SessionId::main(), c, provider, tx, store, oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));
    handle.submit("你好".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    let system = reqs[0].system.as_deref().unwrap_or("");
    assert!(system.contains("XXXPDFSKILL"), "技能正文应注入系统提示词: {system}");
}

#[tokio::test]
async fn history_is_passed_as_messages() {
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("知道了"));
    let captures = provider.captures();
    let store = oc_store::Store::open_memory().unwrap();

    let handle = session::spawn(oc_proto::SessionId::main(), cfg("人格"), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));
    handle.submit("第一句".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    // 第二轮：请求里应带上第一轮的历史。
    let mut rx2 = rx.resubscribe();
    handle.submit("第二句".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx2, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    assert_eq!(reqs.len(), 2, "应发起两次模型请求");
    let second = &reqs[1];
    let joined: Vec<&str> = second.messages.iter().map(|m| m.content.as_str()).collect();
    assert!(joined.contains(&"第一句"), "第二轮应含首轮用户消息: {joined:?}");
    assert!(joined.contains(&"知道了"), "第二轮应含首轮助手回复: {joined:?}");
    assert!(joined.contains(&"第二句"), "第二轮应含本轮用户消息: {joined:?}");
}
