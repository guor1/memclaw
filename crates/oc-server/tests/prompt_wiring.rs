//! M5 第 3 段：验证 oc-core::prompt 的组装结果真的到达模型请求。
//!
//! 补 M3 欠账（render_system_prompt 之前未被调用）的回归测试。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;
use oc_server::testing::{test_cfg, SessionConfigExt};

fn cfg(soul: &str) -> SessionConfig {
    test_cfg().with_soul(soul)
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
    c.skills = vec![oc_core::skill::Skill {
        name: "pdf".into(),
        description: "生成 PDF".into(),
        body: "正文不该进提示词".into(),
        fingerprint: oc_core::skill::fingerprint("正文不该进提示词"),
        enabled: true,
        os: vec![],
    }];

    let handle = session::spawn(oc_proto::SessionId::main(), c, provider, tx, store, oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));
    handle.submit("你好".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    let system = reqs[0].system.as_deref().unwrap_or("");
    assert!(system.contains("pdf"), "技能名应注入: {system}");
    assert!(!system.contains("正文不该进提示词"), "正文不得注入: {system}");
}

/// 「你用的什么模型」必须能答上来：模型名与 provider 须到达 system 提示词。
///
/// 回归点：这三项（模型名/provider/端点）曾经一个都不注入，模型被问到只能答
/// 「系统没告诉我」。它们全在进程内现成可取，不该让模型去读配置文件——读文件既多
/// 一轮工具往返，又会把 config 里的 inline API key 带进 transcript 并逐轮回喂。
#[tokio::test]
async fn model_identity_reaches_model_request() {
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    let store = oc_store::Store::open_memory().unwrap();

    let mut c = cfg("人格");
    c.model = "doubao-seed-1-6-250615".into();

    let handle = session::spawn(oc_proto::SessionId::main(), c, provider, tx, store, oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));
    handle.submit("你用的什么模型".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    let system = reqs[0].system.as_deref().unwrap_or("");
    assert!(
        system.contains("当前模型：doubao-seed-1-6-250615"),
        "模型名应注入系统提示词: {system}"
    );
    // provider 取自 provider 实例本身，不是配置——回退成 mock 时也会如实显示。
    assert!(system.contains("provider: mock-capture"), "provider 标识应注入: {system}");
    // 发出去的请求体 model 字段与提示词里报的应是同一个串（不漂移）。
    assert_eq!(reqs[0].model, "doubao-seed-1-6-250615");
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
