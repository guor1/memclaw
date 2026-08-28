//! M3 防卡死一闸测试：空闲看门狗 / abort / 多轮对话。
//!
//! 直接用 session actor + broadcast 事件，不经传输层，以便精确断言时序。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::{Delta, FinishReason};
use oc_proto::{Event, LifecyclePhase, RunErrorKind};
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;

fn cfg(idle_ms: u64) -> SessionConfig {
    SessionConfig {
        model: "mock".into(),
        system_prompt: None,
        idle_timeout: Duration::from_millis(idle_ms),
        run_timeout: None,
        queue_cap: 8,
        tools: None,
    }
}

/// 收集事件直到出现终态（End 或 Error），或超时。
async fn collect_until_terminal(
    rx: &mut broadcast::Receiver<Event>,
    timeout: Duration,
) -> Vec<Event> {
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
                if terminal {
                    break;
                }
            }
            _ => break,
        }
    }
    out
}

#[tokio::test]
async fn happy_multi_turn() {
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(MockProvider::echo_text("回复A"));
    let handle = session::spawn(cfg(2000), provider, tx);

    // 第一轮
    let _run1 = handle.submit("你好".into()).await.expect("run1");
    let evs = collect_until_terminal(&mut rx, Duration::from_secs(2)).await;
    assert!(has_assistant_containing(&evs, "回复A"), "应收到 assistant 文本");
    assert!(has_end(&evs), "应正常结束");

    // 第二轮（车道空闲后应能再次运行）
    let _run2 = handle.submit("再来".into()).await.expect("run2");
    let evs = collect_until_terminal(&mut rx, Duration::from_secs(2)).await;
    assert!(has_end(&evs), "第二轮应正常结束");
}

#[tokio::test]
async fn idle_watchdog_aborts_stalled_model() {
    let (tx, mut rx) = broadcast::channel(256);
    // provider 在首个 Delta 前卡 5s，但 idle 超时设 200ms。
    let provider = Arc::new(MockProvider::stalls_for(Duration::from_secs(5)));
    let handle = session::spawn(cfg(200), provider, tx);

    let _run = handle.submit("会卡住".into()).await.expect("run");
    let start = std::time::Instant::now();
    let evs = collect_until_terminal(&mut rx, Duration::from_secs(3)).await;
    let elapsed = start.elapsed();

    // 应远早于 5s 被看门狗中止。
    assert!(elapsed < Duration::from_secs(2), "看门狗应快速中止，实际 {elapsed:?}");
    assert!(has_error(&evs), "应以错误/超时终态结束");
}

#[tokio::test]
async fn abort_stops_active_run() {
    let (tx, mut rx) = broadcast::channel(256);
    // 慢速脚本：多个 Delta 每隔 300ms，给 abort 时间介入。
    let script = vec![
        ScriptStep { delay: Duration::from_millis(300), delta: Delta::Text("part1".into()) },
        ScriptStep { delay: Duration::from_millis(300), delta: Delta::Text("part2".into()) },
        ScriptStep { delay: Duration::from_millis(300), delta: Delta::Done(FinishReason::Stop) },
    ];
    let provider = Arc::new(MockProvider::scripted(script));
    let handle = session::spawn(cfg(2000), provider, tx);

    let run_id = handle.submit("长回复".into()).await.expect("run");

    // 等一小会让 run 跑起来，然后中止。
    tokio::time::sleep(Duration::from_millis(150)).await;
    handle.abort(run_id, false).await;

    let evs = collect_until_terminal(&mut rx, Duration::from_secs(2)).await;
    assert!(has_error(&evs), "abort 应产生错误终态");
}

fn has_assistant_containing(evs: &[Event], needle: &str) -> bool {
    evs.iter().any(|e| matches!(e, Event::Assistant { delta, .. } if delta.contains(needle)))
}
fn has_end(evs: &[Event]) -> bool {
    evs.iter().any(|e| matches!(e, Event::Lifecycle { phase: LifecyclePhase::End, .. }))
}
fn has_error(evs: &[Event]) -> bool {
    evs.iter().any(|e| matches!(
        e,
        Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
    ))
}

// 用到 RunErrorKind 以确保导出可见（防未使用告警）。
#[allow(dead_code)]
fn _assert_kind(k: RunErrorKind) -> RunErrorKind { k }
