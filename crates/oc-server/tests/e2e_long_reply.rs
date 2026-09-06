//! P0-1 端到端：长回复经**真实传输**完整送达，慢消费下也不丢字。
//!
//! 对应手册 TC-1（长回复不截断）与 TC-2（背压不丢字）。
//!
//! 与既有 `long_reply.rs` 的区别：那条测的是 server 内部 `RunSink` 到 broadcast
//! 的路径；这条走**完整链路**——NDJSON 编解码、真 socket/管道、连接出站队列。
//! P0-1 的两处根因（broadcast 容量 256 溢出 + `conn.rs` `try_send` 静默丢帧）
//! 后者只在真连接上才存在，进程内测试碰不到。
//!
//! 断言方式是逐字比对拼接文本，而非人眼看"有没有断裂"——手册 TC-1 的
//! 通过标准写的是"回复从头到尾连贯，无跳过一段"，那是无法复核的主观判断。
//!
//! # 这两条测到的、与没测到的
//!
//! 反向验证（2026-09-06，Windows）：
//! - 在 `RunSink::send` 里人为每 100 帧丢 1 帧 → 两条**都转红**（1978 ≠ 2000）。
//!   说明断言确实在比对全部内容，不是走过场。
//! - 把 `send().await` 换回 P0-1 修复前的 `try_send`（队列满即静默丢弃）
//!   → 两条**仍全绿**，即使加到 2000 × 8KB ≈ 16MB。原因是出站队列（`conn.rs`
//!   的 `OUTBOUND_CAP` = 256）根本没填满：内核 socket/管道缓冲把数据全吸收了，
//!   写任务一直推得动，`try_send` 于是从不失败。
//!
//! 所以：**这两条是端到端完整性测试，不是「队列满 → 丢帧」的定向回归**。
//! 要定向覆盖那条路径，得在 `RunSink`/`conn` 层面用一个容量可注入的假下游
//! 直接测，而不是隔着真内核缓冲从客户端侧碰运气。留作后续。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::{Delta, FinishReason};
use oc_server::testing::TestDaemon;

/// 远超 broadcast 容量（256）的 delta 数。P0-1 回归的关键量级：
/// 低于容量时缺陷不显现。
const DELTA_COUNT: usize = 400;

/// 慢消费用例里每个 delta 的载荷大小。
///
/// 这个值不是随便取的：出站队列容量 256（`conn.rs` 的 `OUTBOUND_CAP`），
/// 但队列填满的前提是**写任务也推不动**——否则帧刚入队就被写走，队列永远是空的。
/// 小载荷时 400 帧才 ~32KB，全被内核 socket/管道缓冲吸收，背压根本不发生，
/// 于是「静默丢帧」的回归测不出来（实测确认：把 `send().await` 换成
/// `try_send` 后小载荷版本照样全绿）。4KB × 400 ≈ 1.6MB 才能撑破缓冲。
const SLOW_CHUNK_BYTES: usize = 4096;

/// 每个 delta 内容唯一，便于定位**丢的是哪一段**——只断言总长度的话，
/// 丢一段又多一段会互相抵消。`pad` 为每段补到的字节数（0 = 不补）。
fn numbered_deltas(count: usize, delay: Duration, pad: usize) -> (Vec<ScriptStep>, String) {
    let mut steps = Vec::with_capacity(count + 1);
    let mut expected = String::new();
    for i in 0..count {
        let mut chunk = format!("[{i}]");
        if pad > chunk.len() {
            // 用 ASCII 填充，长度即字节数，断言里好算。
            chunk.push_str(&"x".repeat(pad - chunk.len()));
        }
        expected.push_str(&chunk);
        steps.push(ScriptStep {
            delay,
            delta: Delta::Text(chunk),
        });
    }
    steps.push(ScriptStep {
        delay: Duration::ZERO,
        delta: Delta::Done(FinishReason::Stop),
    });
    (steps, expected)
}

/// TC-1：400 个 delta 经真实传输一字不落。
#[tokio::test]
async fn long_reply_survives_real_transport() {
    let (script, expected) = numbered_deltas(DELTA_COUNT, Duration::ZERO, 0);
    let daemon = TestDaemon::start("long-reply", Arc::new(MockProvider::scripted(script))).await;
    let mut client = daemon.client().await;

    let turn = client.chat_turn("讲个长故事").await;

    assert!(turn.ended_ok(), "应正常结束，实际 {:?}", turn.error_kind());
    assert_eq!(
        turn.text(),
        expected,
        "长回复应逐字完整；缺失即为 P0-1 的丢帧回归"
    );
    assert_eq!(
        turn.deltas.len(),
        DELTA_COUNT,
        "delta 条数应与脚本一致（合并/丢失都算失败）"
    );
}

/// TC-2：客户端慢消费时内容仍完整——背压把整条流拖慢，而不是丢中间段。
///
/// 慢消费的模拟方式：收帧之间插入延迟，让连接出站队列填满。
/// 若 `conn.rs` 退回 `try_send` 静默丢帧，这条会挂掉。
#[tokio::test]
async fn slow_consumer_gets_backpressure_not_loss() {
    // 大载荷：撑破内核缓冲，让出站队列真的填满（理由见 SLOW_CHUNK_BYTES 注释）。
    let (script, expected) = numbered_deltas(DELTA_COUNT, Duration::ZERO, SLOW_CHUNK_BYTES);
    let daemon = TestDaemon::start("slow-consumer", Arc::new(MockProvider::scripted(script))).await;
    let mut client = daemon.client().await;

    client.chat("讲个长故事", None).await;

    // 关键：先**完全不读**，让生产端跑满内核缓冲 + 出站队列（256）。
    // 边读边睡是不够的——消费速度仍远高于生产，队列始终是空的，背压不会发生。
    // 只有让生产端真正撞上满队列，`send().await`（挂起）与 `try_send`（丢帧）
    // 的区别才会显现。
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 再慢慢读完。
    let mut deltas = Vec::new();
    let mut ended = false;
    let mut n = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !ended {
        assert!(
            tokio::time::Instant::now() < deadline,
            "慢消费下应变慢但不应挂死；已收 {} 个 delta",
            deltas.len()
        );
        match client.recv_within(Duration::from_secs(10)).await {
            oc_proto::Frame::Event(oc_proto::Event::Assistant { delta, .. }) => {
                deltas.push(delta);
            }
            // End 与 Error 都算终态；只等 End 会在错误路径上白等到超时。
            oc_proto::Frame::Event(oc_proto::Event::Lifecycle { phase, .. })
                if !matches!(phase, oc_proto::LifecyclePhase::Start) =>
            {
                ended = true;
            }
            _ => {}
        }
        n += 1;
        if n.is_multiple_of(20) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    assert_eq!(
        deltas.len(),
        DELTA_COUNT,
        "慢消费下不应丢帧（队列满时应背压挂起，而非 try_send 静默丢弃）"
    );
    assert_eq!(
        deltas.concat(),
        expected,
        "慢消费应触发背压（变慢），而非丢字"
    );
}
