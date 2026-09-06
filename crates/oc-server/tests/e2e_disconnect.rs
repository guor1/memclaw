//! 断连收敛与会话隔离：run 中途客户端消失、以及慢工具不拖垮其它会话。
//!
//! 对应手册 TC-7（断连不锁车道）、TC-P1-1e（ask_user 等待中断连）、
//! TC-6（大目录 grep 不冻结其它会话）。
//!
//! 这三条的共同点是**判定依赖运行时状态而非回复内容**——手册里的做法是
//! "关掉 TUI，再跑 `oc debug` 看会话是否回到 idle"。`oc debug` 拿的就是
//! `Method::Diagnostics`，所以这里直接断言那份快照，比人眼读终端可靠。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::{Delta, FinishReason};
use oc_server::testing::{SessionConfigExt, TestDaemon};

/// 一轮慢回复：占道足够久，好让我们在 run 进行中断连。
fn slow_reply(delay: Duration) -> Vec<ScriptStep> {
    vec![
        ScriptStep {
            delay,
            delta: Delta::Text("回复".into()),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Done(FinishReason::Stop),
        },
    ]
}

/// 持续吐字的长回复：每 `gap` 出一段，共 `n` 段。
///
/// 与 [`slow_reply`] 的区别很关键：这个在**流式进行中**，
/// 每段都会走一次 `emit_inline` → 断连能被立刻探测到。
fn streaming_reply(n: usize, gap: Duration) -> Vec<ScriptStep> {
    let mut steps: Vec<_> = (0..n)
        .map(|i| ScriptStep {
            delay: gap,
            delta: Delta::Text(format!("第{i}段。")),
        })
        .collect();
    steps.push(ScriptStep {
        delay: Duration::ZERO,
        delta: Delta::Done(FinishReason::Stop),
    });
    steps
}

/// 轮询诊断直到 `main` 会话车道空闲，或超时。
///
/// 返回是否在时限内收敛。轮询而非固定 sleep：收敛是异步的，
/// 固定等待要么太短（假失败）要么太长（拖慢测试）。
async fn wait_lane_idle(daemon: &TestDaemon, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        // 每次用**新连接**查：老连接正是被我们断掉的那条。
        let mut probe = daemon.client().await;
        let diag = probe.diagnostics().await;
        let busy = diag
            .sessions
            .iter()
            .find(|s| s.session_id.as_str() == "main")
            .map(|s| s.active.is_some() || s.lane_busy_since.is_some())
            .unwrap_or(false);
        if !busy {
            return true;
        }
        drop(probe);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// TC-7：**流式进行中**客户端断开，车道应立即释放，不被死连接锁住。
///
/// 收敛机制：`emit_inline` 往断掉的 sink 发事件会失败 → 触发 `cancel`
/// （见 run.rs）。所以必须在**有事件持续外发**时断连才验得到——
/// 这也正是手册 TC-7 描述的场景（"回复途中直接关闭 TUI"）。
#[tokio::test]
async fn disconnect_while_streaming_releases_lane() {
    let daemon = TestDaemon::builder(
        "disc-lane",
        // 每 200ms 一段、共 200 段 ≈ 40s：远长于下面 8s 的等待窗，
        // 保证"车道空了"只可能来自断连收敛，而非 run 自己跑完。
        // （实测教训：早先用 5s 的单段脚本 + 10s 等待窗，把断连收敛逻辑
        // 整个删掉也照样绿——那样的用例什么都证明不了。）
        Arc::new(MockProvider::scripted(streaming_reply(
            200,
            Duration::from_millis(200),
        ))),
    )
    // 空闲看门狗设得远长于本用例，确保释放来自**断连收敛**而非超时兜底。
    .map_cfg(|c| c.with_idle_timeout(Duration::from_secs(120)))
    .start()
    .await;

    {
        let mut client = daemon.client().await;
        client.chat("讲个长故事", None).await;
        // 等 run 起步并开始吐字再断——太早断的话还没占道，测不到东西。
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut probe = daemon.client().await;
        let diag = probe.diagnostics().await;
        assert!(
            diag.sessions
                .iter()
                .any(|s| s.session_id.as_str() == "main" && s.active.is_some()),
            "断连前应有活跃 run，否则本用例什么都没验到：{:?}",
            diag.sessions
        );
    } // client 在此 drop → 连接关闭

    assert!(
        wait_lane_idle(&daemon, Duration::from_secs(8)).await,
        "流式中断连后车道应立即释放（sink 发送失败 → cancel），实际仍被占用"
    );

    // 车道真的可用：重连后新的一轮能起步。
    //
    // 只验"起步"而非"跑完"：`MockProvider::scripted` 每轮都回同一份脚本，
    // 跑完要 40s。能拿到 run_id + Lifecycle::Start 就说明车道确实空了——
    // 这正是 wait_lane_idle 之外想补的那点信息。
    let mut again = daemon.client().await;
    again.chat("你好", None).await;
    let mut started = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !started {
        if let oc_proto::Frame::Event(oc_proto::Event::Lifecycle {
            phase: oc_proto::LifecyclePhase::Start,
            ..
        }) = again.recv_within(Duration::from_secs(5)).await
        {
            started = true;
        }
    }
    assert!(started, "重连后新一轮应能起步，说明车道确实已释放");
}

/// 断连发生在**首个 delta 之前**（run 卡在等模型）时，也应立即收敛。
///
/// 这是 TC-7 的**静默期版本**，与它互补：TC-7 断在流式途中，靠 `emit_inline`
/// 发送失败探测；这条断在等模型期间——那段时间没有任何事件外发，
/// send 探测**根本不会被调用**，只能靠 `RunSink::closed()` 主动等断连。
///
/// 该窗口一度是真实缺口：车道要占到空闲看门狗超时才释放（生产默认
/// `idle_cloud_secs = 120`，即最长 2 分钟）。现由等模型的 `select!` 叠
/// `sink.closed()` 覆盖（与 ask_user / 审批等待同一套机制）。
///
/// `idle_timeout` **刻意设得远长于等待窗**：这样"车道空了"只可能来自断连
/// 收敛，不可能是看门狗兜底——否则把收敛逻辑删掉本用例照样绿。
#[tokio::test]
async fn disconnect_before_first_delta_converges_fast() {
    let daemon = TestDaemon::builder(
        "disc-silent",
        // 60s 不吐字：整段等待期都没有事件外发。
        Arc::new(MockProvider::scripted(slow_reply(Duration::from_secs(60)))),
    )
    .map_cfg(|c| c.with_idle_timeout(Duration::from_secs(120)))
    .start()
    .await;

    {
        let mut client = daemon.client().await;
        client.chat("讲个长故事", None).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut probe = daemon.client().await;
        let diag = probe.diagnostics().await;
        assert!(
            diag.sessions
                .iter()
                .any(|s| s.session_id.as_str() == "main" && s.active.is_some()),
            "断连前应有活跃 run 卡在等模型，否则本用例什么都没验到：{:?}",
            diag.sessions
        );
    } // 断连

    assert!(
        wait_lane_idle(&daemon, Duration::from_secs(5)).await,
        "等模型期间断连后车道应立即释放（sink.closed() → cancel），\
         而非占到空闲看门狗超时"
    );
}

/// TC-8 的传输层版本：两个会话并发，事件各归其位。
///
/// `multi_session.rs` 已在进程内验过归属；这条走真连接，
/// 额外覆盖 NDJSON 编解码与按连接分发。
#[tokio::test]
async fn sessions_do_not_cross_talk_over_transport() {
    let daemon = TestDaemon::start(
        "disc-isolate",
        Arc::new(MockProvider::echo_text("回复内容")),
    )
    .await;

    let mut a = daemon.client().await;
    let mut b = daemon.client().await;

    a.chat("给 A 的话", Some(oc_proto::SessionId::new("sess-a")))
        .await;
    let ta = a.collect_turn(Duration::from_secs(10)).await;

    b.chat("给 B 的话", Some(oc_proto::SessionId::new("sess-b")))
        .await;
    let tb = b.collect_turn(Duration::from_secs(10)).await;

    // 每个连接只应看到自己会话的事件。
    for ev in &ta.events {
        if let oc_proto::Event::Assistant { session, .. } = ev {
            assert_eq!(session.as_str(), "sess-a", "A 连接收到了别的会话的事件");
        }
    }
    for ev in &tb.events {
        if let oc_proto::Event::Assistant { session, .. } = ev {
            assert_eq!(session.as_str(), "sess-b", "B 连接收到了别的会话的事件");
        }
    }
    assert!(ta.ended_ok() && tb.ended_ok());
}

/// TC-6：一个会话在跑慢活时，另一个会话仍能及时得到响应。
///
/// 原手册用"大目录 grep"制造阻塞，判定标准是人眼看"会话 B 能立即开始回复"。
/// 这里用一个慢模型轮等价地占住会话 A，断言会话 B 的**端到端耗时**远小于
/// A 的占用时长——阻塞若真的发生，B 会被拖到和 A 同一量级。
///
/// 注：`grep`/`glob` 的 `spawn_blocking` 包装本身由 oc-tools 侧保证；
/// 这条验的是「会话之间不共享执行资源」这个更上层的不变量。
#[tokio::test]
async fn slow_session_does_not_block_others() {
    const SLOW: Duration = Duration::from_secs(3);

    let daemon = TestDaemon::start(
        "disc-nonblock",
        // 每轮都慢：A 占住 3s，B 也会慢 3s——所以不能比 B 的绝对耗时，
        // 而要看 B 是否**与 A 并行**推进（下面用重叠时间窗判断）。
        Arc::new(MockProvider::scripted(slow_reply(SLOW))),
    )
    .await;

    let mut a = daemon.client().await;
    let mut b = daemon.client().await;

    let t0 = std::time::Instant::now();
    a.chat("慢活", Some(oc_proto::SessionId::new("busy")))
        .await;
    b.chat("快问", Some(oc_proto::SessionId::new("other")))
        .await;

    let ta = a.collect_turn(Duration::from_secs(20)).await;
    let tb = b.collect_turn(Duration::from_secs(20)).await;
    let elapsed = t0.elapsed();

    assert!(ta.ended_ok() && tb.ended_ok(), "两个会话都应正常结束");

    // 并行的话总耗时 ≈ 一轮（3s）；被串行化的话 ≈ 两轮（6s）。
    // 取 1.8 倍作为分界，留足调度抖动余量。
    assert!(
        elapsed < SLOW.mul_f32(1.8),
        "两个会话应并行执行（各 {SLOW:?}），实际总耗时 {elapsed:?} —— 疑似被串行化"
    );
}
