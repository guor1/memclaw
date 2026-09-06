//! P2-3 内存 GC：幂等缓存 TTL + 会话 actor 空闲淘汰。
//!
//! 钉住的不变量分两组。
//!
//! **幂等键**（推时钟测）：TTL 内命中、TTL 外不命中；且冷键——写进去就再没人查的
//! 那些——也会被定期清扫收掉，不能只在 `get` 时才删。
//!
//! **会话 actor**（真实时钟 + 注入短阈值测）：闲置超阈值的子会话被淘汰、`main`
//! 永不淘汰、正忙的不淘汰、心跳自己的扫描不算「活动」、被淘汰的会话下次说话能
//! 重建并带回历史。
//!
//! 为什么两组用不同的时间策略：幂等键那侧只有 map 读写，推时钟安全；会话那侧
//! 涉及 actor 任务、模型轮与看门狗定时器，`start_paused` 的自动推进会把这些
//! 定时器一起拨响（如空闲看门狗中止 run），测的东西就不是淘汰了。改用注入
//! `idle_after`（ZERO / 60s）取代推时钟——判定式 `elapsed() < idle_after` 一样被
//! 完整走到，而且不必真等 24 小时。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, CapturingMock};
use oc_proto::{Event, IdemKey, LifecyclePhase, MethodOk, SessionId};
use oc_server::session::SessionConfig;
use oc_server::testing::{test_cfg, test_state, SessionConfigExt};
use tokio::sync::broadcast;

fn cfg() -> SessionConfig {
    test_cfg().with_soul("人格")
}

/// 等到某会话的一轮跑完（End 或 Error）。
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

/// 等到某会话的车道真正空出来。
///
/// **不能用 `End` 事件当这个屏障**：`End` 由 run 任务发出，之后它才给 actor 投
/// `Finished`；actor 处理到那条命令时才清掉活跃 run、释放车道。也就是说收到 `End`
/// 的瞬间 actor 眼里这轮**还在跑**，此刻探淘汰会正确地答「忙」。
///
/// 车道状态的权威读法是诊断快照的 `active`——`diag.run_done()` 正是在 actor 的
/// `Finished` 分支里调的，与「actor 认为自己空了」同一时刻。
async fn wait_lane_free(state: &Arc<oc_server::ServerState>, id: &SessionId, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let free = state
            .diag()
            .snapshot_sessions()
            .iter()
            .find(|s| &s.session_id == id)
            .is_some_and(|s| s.active.is_none() && s.queue_depth == 0);
        if free {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("等车道空出超时：session={id}");
}

// ── 幂等缓存 TTL ────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn idem_entry_expires_after_ttl() {
    let store = oc_store::Store::open_memory().unwrap();
    let (state, _reg, _rx) = test_state(Arc::new(MockProvider::echo_text("ok")), cfg(), store);

    let key = IdemKey::new("k-1");
    state.idem_put(key.clone(), MethodOk::Empty);

    // TTL 内：命中。
    tokio::time::advance(Duration::from_secs(3599)).await;
    assert!(state.idem_get(&key).is_some(), "TTL 内应命中缓存");

    // 过 TTL：不命中，且条目被顺手删掉（不是留着继续占内存）。
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(state.idem_get(&key).is_none(), "过 TTL 应不再命中");
    assert_eq!(state.idem_len(), 0, "过期条目应在 get 时被删除");
}

#[tokio::test(start_paused = true)]
async fn gc_sweeps_cold_expired_idem_keys() {
    let store = oc_store::Store::open_memory().unwrap();
    let (state, _reg, _rx) = test_state(Arc::new(MockProvider::echo_text("ok")), cfg(), store);

    // 三个写进去就不再被查的「冷键」——单靠 get 时删永远收不掉它们。
    for i in 0..3 {
        state.idem_put(IdemKey::new(format!("cold-{i}")), MethodOk::Empty);
    }
    assert_eq!(state.idem_len(), 3);

    // 未到 TTL 的扫描不该动它们。
    tokio::time::advance(Duration::from_secs(60)).await;
    let r = state.gc_tick().await;
    assert_eq!(r.idem_expired, 0, "未到 TTL 不应清理");
    assert_eq!(state.idem_len(), 3);

    // 过 TTL 后扫描：全清。
    tokio::time::advance(Duration::from_secs(3601)).await;
    let r = state.gc_tick().await;
    assert_eq!(r.idem_expired, 3, "过期冷键应被扫描清掉");
    assert_eq!(state.idem_len(), 0);
}

// ── 会话 actor 空闲淘汰 ─────────────────────────────────────────

#[tokio::test]
async fn idle_subsession_is_evicted_main_is_kept() {
    let store = oc_store::Store::open_memory().unwrap();
    let (state, reg, _rx) = test_state(Arc::new(MockProvider::echo_text("ok")), cfg(), store);

    let tmp = SessionId::new("tmp-1");
    let _h = reg.get_or_spawn(&tmp);
    assert_eq!(reg.len(), 2, "main + tmp-1");

    // idle_after = ZERO：任何「无活跃 run、无排队轮」的会话都算闲置。
    let r = state.gc_tick_with(Duration::from_secs(3600), Duration::ZERO).await;
    assert_eq!(r.sessions_evicted, 1, "闲置子会话应被淘汰");
    assert_eq!(reg.len(), 1, "只剩 main");
    assert!(!reg.is_empty(), "main 仍在");

    // 再扫一次不该再淘汰任何东西（main 无论闲多久都留着）。
    let r = state.gc_tick_with(Duration::from_secs(3600), Duration::ZERO).await;
    assert_eq!(r.sessions_evicted, 0, "main 不参与淘汰");
    assert_eq!(reg.len(), 1);
}

#[tokio::test]
async fn eviction_clears_diag_and_usage_slots() {
    let store = oc_store::Store::open_memory().unwrap();
    let (state, reg, _rx) = test_state(Arc::new(MockProvider::echo_text("ok")), cfg(), store);

    let tmp = SessionId::new("tmp-diag");
    let _h = reg.get_or_spawn(&tmp);
    state.set_last_input_tokens(tmp.clone(), 1234);
    assert!(
        state.diag().snapshot_sessions().iter().any(|s| s.session_id == tmp),
        "诊断快照应含该会话"
    );
    assert_eq!(state.last_input_tokens(&tmp), Some(1234));

    state.gc_tick_with(Duration::from_secs(3600), Duration::ZERO).await;

    // 三张按 SessionId 键的 map 要一起收干净——只淘汰 actor 等于把无界增长
    // 从一处搬到另两处。
    assert!(
        !state.diag().snapshot_sessions().iter().any(|s| s.session_id == tmp),
        "淘汰后诊断格位应被清掉"
    );
    assert_eq!(state.last_input_tokens(&tmp), None, "淘汰后用量格位应被清掉");
}

#[tokio::test]
async fn busy_session_is_not_evicted() {
    let store = oc_store::Store::open_memory().unwrap();
    // 首个 delta 前卡一小时：submit 返回后这一轮必然仍在跑。
    let provider = Arc::new(MockProvider::stalls_for(Duration::from_secs(3600)));
    // 看门狗放宽到不会在测试期间中止这轮。
    let (state, reg, _rx) = test_state(provider, cfg().with_idle_timeout(Duration::from_secs(600)), store);

    let busy = SessionId::new("busy");
    let h = reg.get_or_spawn(&busy);
    // submit 的回执发生在 actor 把该轮置为活跃之后，故返回即意味着车道已占用。
    h.submit("在跑".into(), h.broadcast_sink()).await.expect("run 起步");

    // 连 idle_after = ZERO 都不该淘汰它：有活跃 run 时「闲了多久」根本不该被问。
    let r = state.gc_tick_with(Duration::from_secs(3600), Duration::ZERO).await;
    assert_eq!(r.sessions_evicted, 0, "有活跃 run 的会话不应被淘汰");
    assert_eq!(reg.len(), 2, "main + busy 都在");
    assert!(!h.is_closed(), "actor 应仍在运行");
}

#[tokio::test]
async fn recent_activity_blocks_eviction() {
    let store = oc_store::Store::open_memory().unwrap();
    let (state, reg, mut rx) = test_state(Arc::new(MockProvider::echo_text("好")), cfg(), store);

    let id = SessionId::new("recent");
    let h = reg.get_or_spawn(&id);
    h.submit("刚说的话".into(), h.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(10)).await;
    wait_lane_free(&state, &id, Duration::from_secs(10)).await;

    // 这一轮刚结束（毫秒级），阈值 60s 内 → 不淘汰。
    let r = state.gc_tick_with(Duration::from_secs(3600), Duration::from_secs(60)).await;
    assert_eq!(r.sessions_evicted, 0, "刚活动过的会话不应被淘汰");
    assert!(!h.is_closed());

    // 阈值降到 0 后同一会话就该走了——证明上面拦住它的是「活动时刻」而非别的。
    let r = state.gc_tick_with(Duration::from_secs(3600), Duration::ZERO).await;
    assert_eq!(r.sessions_evicted, 1);
    assert!(h.is_closed(), "淘汰后 actor 应已退出");
}

/// 心跳每 tick 都会扫一遍所有会话。若扫描算「活动」，空闲阈值永远到不了，淘汰
/// 机制等于不存在——这是本用例要防的回归。
///
/// **为什么用真实时钟熬 1.2 秒**，而不是 `start_paused` + `advance`：
/// - 阈值不能取 ZERO，否则判定式 `elapsed() < ZERO` 恒为假，时钟被刷新过没有都会
///   淘汰，用例通过得毫无意义（第一版正是如此，改坏实现照样绿）。
/// - 阈值非零就得让时间真的流过去。而推时钟在 actor 路径上不可靠：actor 启动时要
///   `ensure_session`（一次写线程往返），此间 runtime 无待跑任务 → 暂停时钟**自动
///   推进** → 淘汰探针的 2s 超时先响；`last_activity` 反而在 advance 之后才初始化，
///   于是实现正确也判「刚活动过」。第一版就栽在这儿，且时快时慢。
///
/// 屏障同理不能用 `evict_if_idle` 的回执——它对「actor 答不淘汰」和「探针超时」
/// 都返回 `false`，分不开。改用「跑完一轮 + 等车道空出」：actor 处理过 Submit 与
/// Finished 才可能到这一步，故它必然已越过 `ensure_session`。
#[tokio::test]
async fn health_scan_does_not_count_as_activity() {
    const IDLE_AFTER: Duration = Duration::from_secs(1);

    let store = oc_store::Store::open_memory().unwrap();
    let (state, reg, mut rx) = test_state(Arc::new(MockProvider::echo_text("ok")), cfg(), store);

    let id = SessionId::new("scanned");
    let h = reg.get_or_spawn(&id);
    h.submit("起一轮当屏障".into(), h.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(10)).await;
    wait_lane_free(&state, &id, Duration::from_secs(10)).await;

    // 熬过阈值，其间只有心跳扫描，没有任何真实活动。
    tokio::time::sleep(IDLE_AFTER + Duration::from_millis(200)).await;
    // 扫描与随后的淘汰探针走同一条命令通道、按序处理，故探针能看到扫描的后果。
    for _ in 0..3 {
        reg.health_scan_all().await;
    }

    let r = state.gc_tick_with(Duration::from_secs(3600), IDLE_AFTER).await;
    assert_eq!(r.sessions_evicted, 1, "健康扫描不应刷新空闲时钟");
}

/// 注册表里留着一条**已死**句柄时，`get_or_spawn` 必须换新 actor。
///
/// 这是淘汰与「会话正好此刻被唤醒」撞车时的收敛点：actor 已退出、而移除尚未发生
/// （或调用方握着淘汰前取到的旧句柄）。若此时把死句柄交出去，该会话此后**永久**
/// 不可用——`submit()` 恒返回 `None`，dispatch 报「队列已满」，而队列其实空着。
///
/// 直接绕开 `evict_idle` 来构造这个状态：`evict_if_idle` 让 actor 自己退出，但不碰
/// 注册表，于是「条目还在、actor 已死」这个中间态被稳定复现（走 `gc_tick` 反而
/// 复现不了——它移除条目后 `get_or_spawn` 只是走新建分支，覆盖不到这条判定）。
#[tokio::test]
async fn get_or_spawn_replaces_dead_handle() {
    let store = oc_store::Store::open_memory().unwrap();
    let (_state, reg, _rx) = test_state(Arc::new(MockProvider::echo_text("ok")), cfg(), store);

    let id = SessionId::new("stale");
    let dead = reg.get_or_spawn(&id);
    assert!(dead.evict_if_idle(Duration::ZERO).await, "actor 应判定自己空闲并退出");
    assert!(dead.is_closed(), "actor 已退出");
    assert_eq!(reg.len(), 2, "注册表条目仍在（本用例刻意不走 evict_idle 的移除）");

    let fresh = reg.get_or_spawn(&id);
    assert!(!fresh.is_closed(), "应换到可用的新 actor，而非返回死句柄");
    // 新 actor 真的能收活儿。
    fresh.submit("换新之后还能说话".into(), fresh.broadcast_sink()).await.expect("新 actor 应受理");
    assert_eq!(reg.len(), 2, "仍是 main + stale 两条，不该多出一条");
}

#[tokio::test]
async fn evicted_session_respawns_with_history() {
    let store = oc_store::Store::open_memory().unwrap();
    let provider = Arc::new(CapturingMock::new("知道了"));
    let captures = provider.captures();
    let (state, reg, mut rx) = test_state(provider, cfg(), store);

    let id = SessionId::new("revive");
    let h1 = reg.get_or_spawn(&id);
    h1.submit("第一句：记住土豆".into(), h1.broadcast_sink()).await.expect("first");
    wait_terminal(&mut rx, Duration::from_secs(10)).await;
    wait_lane_free(&state, &id, Duration::from_secs(10)).await;

    // 淘汰它。
    let r = state.gc_tick_with(Duration::from_secs(3600), Duration::ZERO).await;
    assert_eq!(r.sessions_evicted, 1);
    assert!(h1.is_closed());

    // 同一 id 再说话：registry 应建**新** actor（而不是返回那条死句柄——
    // 死句柄会让这个会话此后永久返回「队列已满」）。
    let h2 = reg.get_or_spawn(&id);
    assert!(!h2.is_closed(), "应拿到可用的新 actor");
    h2.submit("第二句：刚才说的是什么".into(), h2.broadcast_sink()).await.expect("second");
    wait_terminal(&mut rx, Duration::from_secs(10)).await;

    // 淘汰不丢状态：历史在 SQLite 里，actor 只持有本轮车道状态。新 actor 的
    // 模型请求应能看到淘汰前那句。
    let reqs = captures.lock().unwrap();
    assert_eq!(reqs.len(), 2, "应有两次模型请求");
    let second: Vec<&str> = reqs[1].messages.iter().map(|m| m.content.as_str()).collect();
    assert!(
        second.iter().any(|c| c.contains("记住土豆")),
        "重建后的 actor 应带回淘汰前的历史: {second:?}"
    );
}
