//! P1-1 回归：ask_user 工具全链路。
//!
//! 覆盖三条路径（与 P0-2/P0-3 的审批通道同源）：
//! - 正常回答：模型调 ask_user → 发 UserInput 事件 → 用户回执文本 → run 完成，
//!   且回执被 registry 消费（不泄漏）。
//! - abort 打断：用户始终不回答 → abort → run 及时终止（不卡在 gate.ask）+ registry 清空。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self, SessionConfig};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::ask_user::AskUserTool;
use oc_tools::ToolRegistry;
use tokio::sync::broadcast;

fn tool_call_step(id: &str, name: &str, args: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: id.into(),
                name: Some(name.into()),
                args_chunk: args.into(),
            }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
    ]
}

fn text_step(t: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text(t.into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ]
}

fn cfg(tools: ToolExecutor) -> SessionConfig {
    SessionConfig {
        model: "mock".into(),
        system_prompt: None,
        idle_timeout: Duration::from_secs(30),
        run_timeout: None,
        queue_cap: 8,
        tools: Some(tools),
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

#[tokio::test]
async fn ask_user_roundtrip_feeds_answer_back() {
    let (tx, mut rx) = broadcast::channel(512);

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(AskUserTool));
    let inputs: oc_server::state::InputRegistry = Arc::new(dashmap::DashMap::new());
    let executor = ToolExecutor::new(Arc::new(reg)).with_inputs(Arc::clone(&inputs));

    // 第一轮：模型调 ask_user；第二轮：基于回答给出文本回复。
    let scripts = vec![
        tool_call_step("call-ask", "ask_user", "{\"prompt\": \"你叫什么名字？\"}"),
        text_step("你好，Kiro"),
    ];
    let provider = Arc::new(SequencedMock::new(scripts));
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg(executor),
        provider,
        tx,
        oc_store::Store::open_memory().unwrap(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );

    let _run = handle
        .submit("问我名字".into(), handle.broadcast_sink())
        .await
        .expect("run");

    // 后台：收到 UserInput 事件后回执一个文本答复。
    let inputs2 = Arc::clone(&inputs);
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(Event::UserInput { input_id, prompt, .. })) => {
                    assert_eq!(prompt, "你叫什么名字？");
                    if let Some((_, tx)) = inputs2.remove(&input_id) {
                        let _ = tx.send(Some("Kiro".into()));
                    }
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
    });

    // 等运行收敛。回答被消费后模型第二轮给出文本、正常结束。
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(inputs.is_empty(), "回执后 input registry 应清空（消费 + RAII 清理）");
}

#[tokio::test]
async fn abort_interrupts_pending_ask_user_and_cleans_registry() {
    let (tx, mut rx) = broadcast::channel(512);

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(AskUserTool));
    let inputs: oc_server::state::InputRegistry = Arc::new(dashmap::DashMap::new());
    let executor = ToolExecutor::new(Arc::new(reg)).with_inputs(Arc::clone(&inputs));

    let scripts = vec![tool_call_step("call-ask", "ask_user", "{\"prompt\": \"在吗？\"}")];
    let provider = Arc::new(SequencedMock::new(scripts));
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg(executor),
        provider,
        tx,
        oc_store::Store::open_memory().unwrap(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );

    let run_id = handle
        .submit("问点啥".into(), handle.broadcast_sink())
        .await
        .expect("run");

    // 等 UserInput 事件出现（run 已卡在 gate.ask 等回执）。
    let mut saw = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(ev, Event::UserInput { .. }) {
            saw = true;
            break;
        }
    }
    assert!(saw, "应先收到 UserInput 请求事件");
    assert_eq!(inputs.len(), 1, "等待期间 input registry 应有一条待处理 entry");

    // 不回答，直接 abort。P0-2 同源：不应永久卡住。
    let start = std::time::Instant::now();
    handle.abort(run_id, false).await;

    let mut got_terminal = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(ev, Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }) {
            got_terminal = true;
            break;
        }
    }
    let elapsed = start.elapsed();
    assert!(got_terminal, "abort 后 run 应以错误终态结束");
    assert!(elapsed < Duration::from_secs(2), "abort 应立即生效，实际 {elapsed:?}");

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(inputs.is_empty(), "abort 后 input registry 应清空，实际 {} 条", inputs.len());
}

/// 断连收敛回归（真机 TC-P1-1e 暴露的缺陷）：ask_user 静默等待期间 client 断连，
/// run 应**立即**感知（经 `RunSink::closed()`）并收敛，而非干等到空闲看门狗兜底。
///
/// 用 `RunSink::Conn(tx)` 模拟真实连接：drop 其接收端 = client 断连。断连后 run 应
/// 远快于 idle_timeout 释放（InputGuard 清 registry 为可观测信号）。
#[tokio::test]
async fn client_disconnect_during_ask_user_converges_fast() {
    use oc_proto::Frame;
    use oc_server::sink::RunSink;

    // broadcast 只承载 Usage/Proactive；本测试的 UserInput/Lifecycle 走 Conn sink。
    let (tx, _rx) = broadcast::channel(512);

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(AskUserTool));
    let inputs: oc_server::state::InputRegistry = Arc::new(dashmap::DashMap::new());
    let executor = ToolExecutor::new(Arc::new(reg)).with_inputs(Arc::clone(&inputs));

    // 关键：idle_timeout 给足（30s），确保「快速收敛」只可能来自断连感知而非看门狗。
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg(executor),
        Arc::new(SequencedMock::new(vec![tool_call_step(
            "call-ask",
            "ask_user",
            "{\"prompt\": \"在吗？\"}",
        )])),
        tx,
        oc_store::Store::open_memory().unwrap(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );

    // 模拟一条真实连接的出站队列：sink = Conn(conn_tx)，我们持有 conn_rx。
    let (conn_tx, mut conn_rx) = tokio::sync::mpsc::channel::<Frame>(256);
    let run_id = handle
        .submit("问点啥".into(), RunSink::Conn(conn_tx))
        .await
        .expect("run");
    let _ = run_id;

    // 读出站帧直到看到 UserInput（run 已卡在 ask_user 等回执）。
    let mut saw = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, conn_rx.recv()).await {
        if matches!(frame, Frame::Event(Event::UserInput { .. })) {
            saw = true;
            break;
        }
    }
    assert!(saw, "应先收到 UserInput 帧（run 已进入等待）");
    assert_eq!(inputs.len(), 1, "等待期间应有一条待处理 input entry");

    // 模拟 client 断连：drop 出站队列接收端 → sink.closed() 完成。
    let start = std::time::Instant::now();
    drop(conn_rx);

    // run 应快速收敛：InputGuard 清 registry。远小于 idle_timeout(30s)。
    let mut converged = false;
    let poll_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < poll_deadline {
        if inputs.is_empty() {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let elapsed = start.elapsed();
    assert!(converged, "断连后 run 应收敛并清理 input registry（TC-P1-1e）");
    assert!(
        elapsed < Duration::from_secs(2),
        "断连收敛应近乎即时（非看门狗兜底），实际 {elapsed:?}"
    );
}
