//! P1-6：episodic 记忆产出（设计 §11.5 flush）。
//!
//! 这是 dreaming 的**上游断点**修复：在本项之前全仓没有任何代码写
//! `tier='episodic'`，而 `dream_candidates` 只取 episodic，故真机上 dreaming
//! 始终空转（§11.4 的巩固能力已完整但无输入）。
//!
//! 覆盖两条 flush 触发路径 + 与既有写入路径的边界：
//! 1) compact 摘要前沉淀（内容将被有损摘要取代，这是它进长期记忆的最后机会）
//! 2) session.reset 推进起点前沉淀（之后这段对话不再进任何提示词）
//! 3) 与 `persist_explicit_memory` 不重复（「记住…」已是 curated）
//! 4) 重复 flush 幂等（内容哈希做 id）
//! 5) 落库的候选能被 dreaming 双门取到（闭环验证）

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_proto::SessionId;
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;
use oc_server::testing::test_cfg;

fn cfg() -> SessionConfig {
    test_cfg()
}

/// 预置一段有实质内容的对话（长度须过 `MIN_PAIR_CHARS` 门槛）。
async fn seed_history(w: &oc_store::Writer, session: &str, pairs: usize) {
    w.ensure_session(session.into(), "main".into()).await.unwrap();
    for i in 0..pairs {
        w.append_entry(oc_store::NewEntry {
            session_id: session.into(),
            role: oc_store::Role::User,
            content: format!("第 {i} 个问题：这个项目的记忆分层是怎么设计的？"),
            tokens_est: 12,
        })
        .await
        .unwrap();
        w.append_entry(oc_store::NewEntry {
            session_id: session.into(),
            role: oc_store::Role::Assistant,
            content: format!("回答 {i}：分 curated 与 episodic 两层，前者会话起始注入，后者按需检索。"),
            tokens_est: 20,
        })
        .await
        .unwrap();
    }
}

/// 取库里所有 episodic 记忆（dream_candidates 只取 episodic，正好复用）。
async fn episodic_rows(w: &oc_store::Writer) -> Vec<oc_store::MemoryRow> {
    w.dream_candidates(100).await.unwrap()
}

#[tokio::test]
async fn compact_flushes_episodic_before_summarizing() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    seed_history(w, "main", 6).await;

    // 沉淀前：库里没有任何 episodic —— 这正是 P1-6 之前的真机状态。
    assert!(episodic_rows(w).await.is_empty(), "初始不应有 episodic");

    let provider = Arc::new(CapturingMock::new("这是压缩后的摘要"));
    let (tx, _rx) = broadcast::channel(64);
    let handle = session::spawn(
        SessionId::main(),
        cfg(),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&SessionId::main()),
    );

    handle.compact().await;

    // compact 异步执行，轮询等 episodic 落库。
    let mut rows = Vec::new();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        rows = episodic_rows(w).await;
        if !rows.is_empty() {
            break;
        }
    }

    assert!(!rows.is_empty(), "compact 应产出 episodic 候选（dreaming 的输入）");
    for r in &rows {
        assert_eq!(r.tier, oc_store::Tier::Episodic);
        // 系统从对话推断该记什么 ≠ 用户交代，origin 不能是 Owner（设计 §4.2）。
        assert_eq!(r.origin, oc_store::Origin::Agent, "推断出的记忆不该记为 Owner");
        assert!(r.importance > 0.0 && r.importance <= 0.9, "importance 越界: {}", r.importance);
    }
    // 成对内容应同时含提问与回答（记住的是完整情节，不是半句）。
    assert!(
        rows.iter().any(|r| r.text.contains("记忆分层") && r.text.contains("curated")),
        "候选应保留 user+assistant 成对内容"
    );
}

#[tokio::test]
async fn reset_flushes_episodic_before_advancing() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    seed_history(w, "main", 4).await;

    assert!(episodic_rows(w).await.is_empty());

    // 直接走 reset 前的沉淀入口（dispatch 的 session.reset handler 调用的就是它）。
    session::flush_before_reset(&store, "main", 200).await;

    let rows = episodic_rows(w).await;
    assert!(!rows.is_empty(), "reset 前应沉淀 episodic，否则这段对话永久失去入库机会");

    // 沉淀完再 reset：上下文清空，但记忆留下了。
    w.reset_session("main".into()).await.unwrap();
    assert!(
        w.load_transcript("main".into(), 100).await.unwrap().is_empty(),
        "reset 后上下文应为空"
    );
    assert!(!episodic_rows(w).await.is_empty(), "reset 不应带走已沉淀的记忆");
}

#[tokio::test]
async fn explicit_memory_not_duplicated_as_episodic() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    // 「记住…」由 persist_explicit_memory 写成 curated；不该再以 episodic 存一份，
    // 否则同一条事实在库里两个 tier 各一份，dreaming 会把它再"巩固"一次。
    w.append_entry(oc_store::NewEntry {
        session_id: "main".into(),
        role: oc_store::Role::User,
        content: "记住：我喜欢简洁直接的回复，不要铺垫".into(),
        tokens_est: 10,
    })
    .await
    .unwrap();
    w.append_entry(oc_store::NewEntry {
        session_id: "main".into(),
        role: oc_store::Role::Assistant,
        content: "好的，已经记下了，之后我会直接给结论。".into(),
        tokens_est: 12,
    })
    .await
    .unwrap();

    session::flush_before_reset(&store, "main", 200).await;

    let rows = episodic_rows(w).await;
    assert!(
        !rows.iter().any(|r| r.text.contains("我喜欢简洁直接的回复")),
        "显式记忆不该重复沉淀为 episodic: {rows:?}"
    );
}

#[tokio::test]
async fn repeated_flush_is_idempotent() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    seed_history(w, "main", 3).await;

    session::flush_before_reset(&store, "main", 200).await;
    let first = episodic_rows(w).await.len();
    assert!(first > 0);

    // 同一段历史再刷一次：id 是内容哈希，upsert 落回同一行，不产生重复候选。
    session::flush_before_reset(&store, "main", 200).await;
    let second = episodic_rows(w).await.len();
    assert_eq!(first, second, "重复 flush 不应产生重复候选");
}

#[tokio::test]
async fn short_chitchat_produces_no_candidates() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    for (role, text) in [
        (oc_store::Role::User, "你好"),
        (oc_store::Role::Assistant, "你好！"),
        (oc_store::Role::User, "谢谢"),
        (oc_store::Role::Assistant, "不客气"),
    ] {
        w.append_entry(oc_store::NewEntry {
            session_id: "main".into(),
            role,
            content: text.into(),
            tokens_est: 2,
        })
        .await
        .unwrap();
    }

    session::flush_before_reset(&store, "main", 200).await;

    assert!(
        episodic_rows(w).await.is_empty(),
        "寒暄不该占用记忆——它会稀释 dreaming 的候选池"
    );
}

#[tokio::test]
async fn flushed_candidates_reach_dreaming_gate() {
    // 闭环验证：flush 产出的候选，形状上确实能进 dreaming 的双门判定。
    // （能否通过门1取决于 use_count/age，真机需时间累积；这里验证 tier/origin
    //  两个**结构性**条件对——它们错了就永远进不了门，与时间无关。）
    use oc_core::dreaming::{gate_check, DreamCandidate, DreamCfg, GateReject};
    use oc_core::memory::{Origin as CoreOrigin, Tier as CoreTier};

    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    seed_history(w, "main", 3).await;
    session::flush_before_reset(&store, "main", 200).await;

    let rows = episodic_rows(w).await;
    assert!(!rows.is_empty());

    let cfg = DreamCfg::default();
    for r in &rows {
        let cand = DreamCandidate {
            id: r.id.clone(),
            tier: match r.tier {
                oc_store::Tier::Curated => CoreTier::Curated,
                oc_store::Tier::Episodic => CoreTier::Episodic,
                oc_store::Tier::Prospective => CoreTier::Prospective,
                oc_store::Tier::Review => CoreTier::Review,
            },
            origin: match r.origin {
                oc_store::Origin::Owner => CoreOrigin::Owner,
                oc_store::Origin::Agent => CoreOrigin::Agent,
                oc_store::Origin::Untrusted => CoreOrigin::Untrusted,
                oc_store::Origin::System => CoreOrigin::System,
            },
            importance: r.importance,
            // 给足频次与时长，单独检验结构门。
            use_count: 5,
            age_secs: 10 * 86400,
        };
        let verdict = gate_check(&cand, &cfg);
        assert_ne!(verdict, Err(GateReject::NotEpisodic), "tier 必须是 episodic");
        assert_ne!(
            verdict,
            Err(GateReject::UntrustedOrigin),
            "origin=Agent 不该被结构门排除，否则沉淀的记忆永远无法巩固"
        );
    }
}
