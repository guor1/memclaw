//! P1-5 端到端：cron 到点后，`Proactive` 提醒**真的推到了客户端**。
//!
//! 对应手册 TC-P1-5d。`cron_tool.rs` 已验过表达式解析、时区换算、落库与
//! 下次触发的重排；这条补的是最后一跳——**事件经真连接送达**。
//!
//! 这一跳单测碰不到，而它恰好是 P1-5 首版栽跟头的地方：当时定时提醒根本
//! 没走 cron（退化成 shell 睡眠），"到点没声音"只有真机才看得见。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::MockProvider;
use oc_proto::{Event, Method, ProactiveKind, ProactiveSource};
use oc_server::testing::TestDaemon;

/// 亚分钟一次性提醒：几秒后触发，事件应推到已连接的客户端。
///
/// 用 `@once`（`next_at` 存绝对秒）而非五字段表达式：cron 最小粒度是分钟，
/// 走表达式的话测试要么等最多 60s，要么在分钟边界附近偶发失败。
/// 这也正是 P1-5 真机踩过的坑——"90 秒后提醒"用重复表达式表达，
/// 变成了一条「每日 21:46」。
#[tokio::test]
async fn cron_reminder_reaches_connected_client() {
    // 库要共享：先用它种一条 cron，再交给 daemon。
    let store = oc_store::Store::open_memory().expect("开内存库");

    let fire_at = time_now_secs() + 2;
    store
        .writer()
        .cron_add(oc_store::types::NewCron {
            id: "cron-e2e-push".into(),
            expr: oc_core::proactive::ONCE_EXPR.into(),
            prompt: "提醒我出发".into(),
            tz: "UTC".into(),
            next_at: Some(fire_at),
        })
        .await
        .expect("种 cron");

    let daemon = TestDaemon::builder(
        "cron-push",
        Arc::new(MockProvider::echo_text("该出发了")),
    )
    // 心跳调快：proactive 扫描挂在心跳上，默认 60s 的话要等一分钟。
    .heartbeat(Duration::from_millis(200))
    .store(store)
    .start()
    .await;

    let mut client = daemon.client().await;

    // 等提醒推过来。给足余量：触发 2s + 心跳扫描间隔 + 模型轮。
    // 用 try_recv_within：超时走到下面的 expect，报「没等到提醒」这个具体原因，
    // 而不是让 harness 抛一句「收帧超时」把失败指到别处。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut got = None;
    while tokio::time::Instant::now() < deadline && got.is_none() {
        match client.try_recv_within(Duration::from_secs(5)).await {
            Some(oc_proto::Frame::Event(Event::Proactive {
                kind,
                text,
                source,
                session,
            })) => got = Some((kind, text, source, session)),
            Some(_) => continue,
            None => continue, // 这一轮没帧，继续等到 deadline
        }
    }

    let (kind, text, source, session) =
        got.expect("到点后应收到 Proactive 提醒——「到点没声音」正是 P1-5 的原始缺陷");

    assert!(matches!(kind, ProactiveKind::Reminder), "应为提醒类事件");
    assert!(
        matches!(source, ProactiveSource::Cron { .. }),
        "来源应是 cron，实际 {source:?}"
    );
    // cron 跑在隔离子会话里，但事件归属 main，好让 client 在主视图显示。
    assert_eq!(session.as_str(), "main", "提醒应归属 main 会话");
    assert!(!text.trim().is_empty(), "提醒正文不应为空");

    // 一次性任务触发后应从列表消失（防"次日又响一次"的回归）。
    client.request(Method::CronList).await;
    let listed = loop {
        match client.recv_within(Duration::from_secs(5)).await {
            oc_proto::Frame::Res(res) => break res,
            _ => continue, // 跳过途中的事件帧
        }
    };
    match listed.result {
        oc_proto::ResResult::Ok(oc_proto::MethodOk::CronList(v)) => {
            assert!(v.is_empty(), "一次性任务触发后应已删除，实际还剩 {v:?}");
        }
        other => panic!("期望 CronList，得到 {other:?}"),
    }
}

fn time_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时间早于 epoch")
        .as_secs() as i64
}
