//! M5 第 5 段：记忆写入路径 + dreaming 巩固。
//!
//! 1) 用户显式"记住…" → 写 curated 记忆（origin=Owner），后续相关消息能被 Lane1 注入。
//! 2) dreaming 双门：episodic 候选经 server 调度巩固为 curated；Untrusted 来源被拒。

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
async fn explicit_remember_then_recall() {
    let store = oc_store::Store::open_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好的，记下了"));
    let captures = provider.captures();
    let handle = session::spawn(cfg(), provider, tx, store.clone());

    // 第 1 轮：显式"记住…"，触发 curated 写入。
    handle.submit("记住：我喜欢简洁直接的回复".into()).await.expect("run1");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    // 该记忆应已落库为 curated + Owner。
    let curated = store
        .writer()
        .search_candidates(vec!["简洁".into()], Some(oc_store::Tier::Curated), 10)
        .await
        .unwrap();
    assert_eq!(curated.len(), 1, "显式记忆应写入 curated");
    assert_eq!(curated[0].origin, oc_store::Origin::Owner, "显式指令 origin=Owner");

    // 第 2 轮：相关消息 → Lane1 注入该记忆。
    handle.submit("你平时怎么组织回复的".into()).await.expect("run2");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    let last = reqs.last().unwrap().system.as_deref().unwrap_or("");
    assert!(
        last.contains("我喜欢简洁直接的回复"),
        "记住的内容应被后续消息召回注入: {last}"
    );
}

#[tokio::test]
async fn dreaming_consolidates_qualified_and_rejects_untrusted() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();

    // 合格 episodic 候选：Agent 来源、高分、够旧（age 由 created_at 决定，
    // 内存库 created_at=now，age≈0 会被 TooRecent 拒；故用 max_age_secs=0 + min_age_secs=0 的自定义 cfg。
    // 这里直接调 server::dreaming::scan，用一个放宽时间窗的 cfg。
    w.upsert_memory(oc_store::NewMemory {
        id: "ep-good".into(),
        tier: oc_store::Tier::Episodic,
        origin: oc_store::Origin::Agent,
        text: "用户常在周五做复盘".into(),
        keywords: None,
        importance: 0.8,
        content_hash: "h1".into(),
    })
    .await
    .unwrap();
    // 频次门：需要 use_count ≥ 2。touch 两次。
    let now = 0i64;
    w.touch_memory("ep-good".into(), now).await.unwrap();
    w.touch_memory("ep-good".into(), now).await.unwrap();

    // 不可信来源：即便高分高频也应被门2排除。
    w.upsert_memory(oc_store::NewMemory {
        id: "ep-bad".into(),
        tier: oc_store::Tier::Episodic,
        origin: oc_store::Origin::Untrusted,
        text: "网页声称的所谓事实".into(),
        keywords: None,
        importance: 0.9,
        content_hash: "h2".into(),
    })
    .await
    .unwrap();
    w.touch_memory("ep-bad".into(), now).await.unwrap();
    w.touch_memory("ep-bad".into(), now).await.unwrap();

    // 放宽时间窗（内存库 age≈0），其余用默认。
    let dream_cfg = oc_core::dreaming::DreamCfg {
        min_age_secs: 0,
        max_age_secs: 0,
        ..oc_core::dreaming::DreamCfg::default()
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let promoted = oc_server::dreaming::scan(&store, now_secs, &dream_cfg).await;
    assert_eq!(promoted, 1, "只应巩固合格的一条");

    // ep-good 已巩固为 curated。
    let curated = w
        .search_candidates(vec!["复盘".into()], Some(oc_store::Tier::Curated), 10)
        .await
        .unwrap();
    assert_eq!(curated.len(), 1);
    assert_eq!(curated[0].id, "ep-good");

    // ep-bad 仍是 episodic（未被巩固）。
    let still_ep = w.dream_candidates(10).await.unwrap();
    assert!(
        still_ep.iter().any(|m| m.id == "ep-bad"),
        "不可信来源应保持未巩固"
    );
    assert!(
        !still_ep.iter().any(|m| m.id == "ep-good"),
        "合格记忆应已离开 episodic 池"
    );
}
