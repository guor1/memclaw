//! M4 工具循环测试：模型请求工具 → 执行 → 结果回喂 → 再次调用模型 → 完成。
//! 以及 loop detection。

use std::sync::Arc;
use std::time::Duration;

use oc_core::tool::ApprovalMode;
use oc_llm::mock::SequencedMock;
use oc_llm::{Delta, FinishReason};
use oc_llm::types::ToolCallDelta;
use oc_llm::mock::ScriptStep;
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self, SessionConfig};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::exec::ExecTool;
use oc_tools::ToolRegistry;
use tokio::sync::broadcast;

fn tool_executor() -> ToolExecutor {
    let mut reg = ToolRegistry::new();
    // Allow 模式：命令直接执行，便于测试闭环（不弹审批）。
    reg.register(Arc::new(ExecTool::new(ApprovalMode::Allow, Duration::from_secs(10))));
    ToolExecutor::new(Arc::new(reg))
}

fn cfg_with_tools(tools: ToolExecutor) -> SessionConfig {
    SessionConfig {
        model: "mock".into(),
        system_prompt: None,
        idle_timeout: Duration::from_secs(5),
        run_timeout: None,
        queue_cap: 8,
        tools: Some(tools),
    }
}

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

async fn collect_until_terminal(rx: &mut broadcast::Receiver<Event>, timeout: Duration) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(ev)) => {
                let terminal = matches!(
                    ev,
                    Event::Lifecycle { phase: LifecyclePhase::End, .. }
                        | Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
                );
                out.push(ev);
                if terminal { break; }
            }
            _ => break,
        }
    }
    out
}

#[tokio::test]
async fn model_calls_tool_then_completes() {
    let (tx, mut rx) = broadcast::channel(512);
    // 第一轮：模型请求 exec echo；第二轮：模型基于结果给出文本回复。
    let echo = "echo tool-ran";
    let scripts = vec![
        tool_call_step("call-1", "exec", &format!("{{\"command\": \"{echo}\"}}")),
        text_step("命令已执行完成"),
    ];
    let provider = Arc::new(SequencedMock::new(scripts));
    let handle = session::spawn(cfg_with_tools(tool_executor()), provider, tx);

    let _run = handle.submit("帮我执行 echo".into()).await.expect("run");
    let evs = collect_until_terminal(&mut rx, Duration::from_secs(5)).await;

    // 应看到 tool start/update/end + 最终 assistant 文本 + End。
    assert!(evs.iter().any(|e| matches!(e, Event::Tool { .. })), "应有工具事件");
    assert!(
        evs.iter().any(|e| matches!(e, Event::Assistant { delta, .. } if delta.contains("命令已执行完成"))),
        "应有最终文本回复"
    );
    assert!(
        evs.iter().any(|e| matches!(e, Event::Lifecycle { phase: LifecyclePhase::End, .. })),
        "应正常结束"
    );
}

#[tokio::test]
async fn loop_detection_breaks_repeated_tool_calls() {
    let (tx, mut rx) = broadcast::channel(512);
    // 模型每轮都请求同一个工具调用（相同参数）→ 应被 loop detection 打断。
    let same = tool_call_step("call-x", "exec", "{\"command\": \"echo loop\"}");
    let scripts = vec![same.clone(), same.clone(), same.clone(), same.clone(), same.clone()];
    let provider = Arc::new(SequencedMock::new(scripts));
    let handle = session::spawn(cfg_with_tools(tool_executor()), provider, tx);

    let _run = handle.submit("触发打转".into()).await.expect("run");
    let evs = collect_until_terminal(&mut rx, Duration::from_secs(5)).await;

    assert!(
        evs.iter().any(|e| matches!(
            e,
            Event::Lifecycle { phase: LifecyclePhase::Error { kind: oc_proto::RunErrorKind::LoopDetected, .. }, .. }
        )),
        "应以 LoopDetected 终态结束"
    );
}
