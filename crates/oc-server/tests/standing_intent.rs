//! P1-2：standing intent（话题触发式待办）端到端。
//!
//! 验证设计 §12.3 的触发链：入站消息词法命中待办关键词 → anti-nagging 判定
//! （cooldown/budget/expiry，逐条用该行自己的参数）→ 允许则注入隐藏上下文提醒
//! 模型 + 抬 fired_count；拒绝则**静默跳过**（不注入、不记账）。
//!
//! 与 cron（到点触发）的分界：本触发链只在主会话入站路径生效。

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
        // 阈值设高，排除 Lane1 记忆注入干扰：本测试只关心 intent 注入。
        trigger_threshold: 0.5,
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 造一条话题待办（关键词「出差」，提醒「带转换插头」）。
async fn seed_intent(
    w: &oc_store::Writer,
    id: &str,
    cooldown_secs: i64,
    budget: u32,
    expiry_at: Option<i64>,
) {
    w.intent_add(oc_store::NewStandingIntent {
        id: id.into(),
        text: "带转换插头".into(),
        keywords: vec!["出差".into(), "德国".into()],
        cooldown_secs,
        budget,
        expiry_at,
    })
    .await
    .unwrap();
}

/// 起一轮对话，返回本轮送给模型的系统提示词。
async fn run_turn(store: &oc_store::Store, msg: &str) -> String {
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好的"));
    let captures = provider.captures();
    let sid = oc_proto::SessionId::main();
    let handle = session::spawn(
        sid.clone(),
        cfg(),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    handle
        .submit(msg.into(), handle.broadcast_sink())
        .await
        .expect("run 应起步");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;
    let reqs = captures.lock().unwrap();
    reqs[0].system.clone().unwrap_or_default()
}

async fn fresh_store() -> oc_store::Store {
    let store = oc_store::Store::open_memory().unwrap();
    store
        .writer()
        .ensure_session("main".into(), "main".into())
        .await
        .unwrap();
    store
}

/// 主路径：话题命中 → 注入提醒 + 抬 fired_count。
#[tokio::test]
async fn matching_topic_injects_reminder_and_marks_fired() {
    let store = fresh_store().await;
    seed_intent(store.writer(), "i1", 86_400, 3, None).await;

    let system = run_turn(&store, "我下周要去德国出差，帮我列个清单").await;

    assert!(
        system.contains("带转换插头"),
        "命中话题应注入待办提醒: {system}"
    );
    assert!(
        system.contains("待办提醒"),
        "注入文本应标注「待办提醒」以与记忆区分: {system}"
    );

    // 记账：fired_count 抬到 1，last_fired_at 落下。
    let rows = store.writer().intent_list().await.unwrap();
    assert_eq!(rows[0].fired_count, 1, "触发后 fired_count 应为 1");
    assert!(rows[0].last_fired_at.is_some(), "应记录 last_fired_at");
}

/// 无关话题 → 不注入、不记账。
#[tokio::test]
async fn unrelated_topic_does_not_inject() {
    let store = fresh_store().await;
    seed_intent(store.writer(), "i1", 86_400, 3, None).await;

    let system = run_turn(&store, "帮我写段排序代码").await;

    assert!(!system.contains("带转换插头"), "无关话题不应注入: {system}");
    let rows = store.writer().intent_list().await.unwrap();
    assert_eq!(rows[0].fired_count, 0, "未触发则 fired_count 应为 0");
    assert!(rows[0].last_fired_at.is_none());
}

/// cooldown 内再次命中 → 静默跳过（这是 anti-nagging 的核心价值）。
#[tokio::test]
async fn cooldown_suppresses_second_trigger() {
    let store = fresh_store().await;
    seed_intent(store.writer(), "i1", 86_400, 3, None).await;

    // 第一轮：正常触发。
    let first = run_turn(&store, "下周去德国出差").await;
    assert!(first.contains("带转换插头"), "首轮应注入: {first}");
    assert_eq!(store.writer().intent_list().await.unwrap()[0].fired_count, 1);

    // 第二轮：同样命中，但在 24h cooldown 内 → 不注入、不再记账。
    let second = run_turn(&store, "出差的机票订好了").await;
    assert!(
        !second.contains("带转换插头"),
        "cooldown 内应静默跳过，避免反复打扰: {second}"
    );
    assert_eq!(
        store.writer().intent_list().await.unwrap()[0].fired_count,
        1,
        "cooldown 拒绝不应抬 fired_count"
    );
}

/// cooldown=0 时连续命中可连续触发，直到 budget 用尽即静默。
#[tokio::test]
async fn budget_exhaustion_silences_intent() {
    let store = fresh_store().await;
    // cooldown=0 排除冷却干扰，budget=2 两次后应静默。
    seed_intent(store.writer(), "i1", 0, 2, None).await;

    let r1 = run_turn(&store, "出差第一次").await;
    assert!(r1.contains("带转换插头"), "第 1 次应注入");
    let r2 = run_turn(&store, "出差第二次").await;
    assert!(r2.contains("带转换插头"), "第 2 次应注入");
    assert_eq!(store.writer().intent_list().await.unwrap()[0].fired_count, 2);

    // budget=2 已用尽 → 第 3 次静默。
    let r3 = run_turn(&store, "出差第三次").await;
    assert!(
        !r3.contains("带转换插头"),
        "budget 用尽后应静默: {r3}"
    );
    assert_eq!(
        store.writer().intent_list().await.unwrap()[0].fired_count,
        2,
        "budget 用尽后不应继续记账"
    );
}

/// 已过期的待办即便命中也不触发。
#[tokio::test]
async fn expired_intent_does_not_trigger() {
    let store = fresh_store().await;
    // expiry_at 落在过去 → 已过期。
    seed_intent(store.writer(), "i1", 0, 3, Some(now_secs() - 10)).await;

    let system = run_turn(&store, "下周去德国出差").await;
    assert!(
        !system.contains("带转换插头"),
        "过期待办不应触发: {system}"
    );
    assert_eq!(store.writer().intent_list().await.unwrap()[0].fired_count, 0);
}

/// 一条消息命中多条待办时，受每轮注入上限约束（默认 ≤3，设计 §12.5）。
#[tokio::test]
async fn per_turn_cap_limits_injection_count() {
    let store = fresh_store().await;
    let w = store.writer();
    // 5 条待办共用同一关键词「出差」，均可触发；每轮上限 3。
    for n in 0..5 {
        w.intent_add(oc_store::NewStandingIntent {
            id: format!("i{n}"),
            text: format!("待办内容{n}"),
            keywords: vec!["出差".into()],
            cooldown_secs: 0,
            budget: 3,
            expiry_at: None,
        })
        .await
        .unwrap();
    }

    let system = run_turn(&store, "下周出差").await;
    let injected = (0..5)
        .filter(|n| system.contains(&format!("待办内容{n}")))
        .count();
    assert_eq!(
        injected, 3,
        "每轮注入应受上限 3 约束，实际 {injected}: {system}"
    );

    // 只有被注入的那 3 条记账。
    let fired: u32 = w.intent_list().await.unwrap().iter().map(|r| r.fired_count).sum();
    assert_eq!(fired, 3, "未注入的待办不应记账");
}
