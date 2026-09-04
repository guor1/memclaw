//! 回归：队列已满时 `submit` 必须返回 `None`，而不是一个永不产生事件的 run_id。
//!
//! 旧 bug：actor 在 `queue.submit()` **之前**就 `reply.send(run_id)`。被
//! `SubmitResult::Rejected` 拒掉的轮子拿到了合法 run_id，却永远不会起步、也就
//!永不产生任何 Lifecycle 事件。调用方若在等它的终态就会白等——oc-http 的
//! `accumulate_response` / SSE 都是无超时的 `recv()` 循环，表现为 HTTP 请求挂死。
//! 默认会话落 main 之后所有 HTTP 请求与 TUI 挤同一条 queue_cap，触发面被放大。
//!
//! 修法：回执挪到队列判定之后，`Rejected` 分支不回执（drop 发送端）→ 调用方
//! `rx.await` 失败 → `None` → dispatch 映射成协议错误，调用方立即知情。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::{Delta, FinishReason};
use oc_proto::SessionId;
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;

/// queue_cap = 1：一个活跃 + 一个排队，第三轮起必被拒。
const QUEUE_CAP: usize = 1;

fn cfg() -> SessionConfig {
    SessionConfig {
        model: "mock".into(),
        system_prompt: None,
        idle_timeout: Duration::from_secs(5),
        run_timeout: None,
        queue_cap: QUEUE_CAP,
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
        context_window: 65536,
    }
}

/// 一轮慢文本：占道足够久，好让后续提交都撞上忙车道。
fn slow_text(t: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep {
            delay: Duration::from_millis(3000),
            delta: Delta::Text(t.into()),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Done(FinishReason::Stop),
        },
    ]
}

#[tokio::test]
async fn queue_full_submit_returns_none() {
    let store = oc_store::Store::open_memory().expect("store");
    let (tx, _rx) = broadcast::channel(512);

    // 给足脚本，避免 mock 耗尽成为失败原因（实际只会用到前两个）。
    let scripts = vec![slow_text("一"), slow_text("二"), slow_text("三")];
    let provider = Arc::new(SequencedMock::new(scripts));
    let handle = session::spawn(
        SessionId::main(),
        cfg(),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&SessionId::main()),
    );

    // 第一轮：起步占道。
    let first = handle.submit("一".into(), handle.broadcast_sink()).await;
    assert!(first.is_some(), "首轮应起步");

    // 确保首轮真的占住车道，后续提交才会进队列而非抢先起步。
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 第二轮：车道忙 → 排队（queue_cap = 1，占满）。
    let second = handle.submit("二".into(), handle.broadcast_sink()).await;
    assert!(second.is_some(), "第二轮应入队，仍返回 run_id");

    // 第三轮：队列已满 → 必须拒绝。
    let third = handle.submit("三".into(), handle.broadcast_sink()).await;
    assert!(
        third.is_none(),
        "队列满时必须返回 None，否则调用方会拿着 run_id 等一个永不存在的 run（挂死）"
    );
}

#[tokio::test]
async fn accepted_runs_get_distinct_run_ids() {
    // 顺带守住：返回 Some 的两轮 run_id 不同（run_id 全局唯一是 abort 定位的前提）。
    let store = oc_store::Store::open_memory().expect("store");
    let (tx, _rx) = broadcast::channel(512);
    let provider = Arc::new(SequencedMock::new(vec![slow_text("一"), slow_text("二")]));
    let handle = session::spawn(
        SessionId::main(),
        cfg(),
        provider,
        tx,
        store,
        oc_server::diag::DiagRegistry::new().for_session(&SessionId::main()),
    );

    let a = handle.submit("一".into(), handle.broadcast_sink()).await.expect("首轮");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let b = handle.submit("二".into(), handle.broadcast_sink()).await.expect("第二轮");
    assert_ne!(a, b);
}
