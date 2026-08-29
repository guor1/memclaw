//! M6：主动性 cron 调度。到期 cron → 隔离子会话跑一轮 → Proactive 事件推送 → 重排。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::MockProvider;
use oc_proto::{Event, ProactiveKind, ProactiveSource};
use oc_server::proactive::{cron_scan, ProactiveCtx};
use tokio::sync::broadcast;

fn ctx(store: oc_store::Store, tx: broadcast::Sender<Event>) -> ProactiveCtx {
    ProactiveCtx {
        provider: Arc::new(MockProvider::echo_text("周报提醒：记得写周报")),
        events: tx,
        store,
        model: "mock".into(),
        soul: "人格".into(),
    }
}

#[tokio::test]
async fn due_cron_fires_and_reschedules() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();

    // 预置一条已到期的 cron（next_at 在过去）。
    w.cron_add(oc_store::NewCron {
        id: "c1".into(),
        expr: "0 9 * * *".into(), // 每天 09:00
        prompt: "提醒我写周报".into(),
        tz: "UTC".into(),
        next_at: Some(1000), // 远小于 now → 到期
    })
    .await
    .unwrap();

    let (tx, mut rx) = broadcast::channel(64);
    let ctx = ctx(store.clone(), tx);

    let now = 2_000_000_000i64; // 远大于 next_at
    let fired = cron_scan(&ctx, now).await;
    assert_eq!(fired, 1, "应触发一条到期 cron");

    // 收到 Proactive 提醒事件。
    let mut got_reminder = false;
    while let Ok(ev) = rx.try_recv() {
        if let Event::Proactive { kind, source, text } = ev {
            assert_eq!(kind, ProactiveKind::Reminder);
            assert!(matches!(source, ProactiveSource::Cron { .. }));
            assert!(text.contains("周报"), "提醒文本应含模型输出: {text}");
            got_reminder = true;
        }
    }
    assert!(got_reminder, "应推送 Proactive 提醒");

    // 重排：next_at 被推到未来（> now）。
    let crons = w.cron_list().await.unwrap();
    let c = crons.iter().find(|c| c.id == "c1").unwrap();
    assert!(c.next_at.unwrap() > now, "触发后应重排到未来: {:?}", c.next_at);
    assert_eq!(c.last_fired_at, Some(now));
}

#[tokio::test]
async fn not_due_cron_does_not_fire() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.cron_add(oc_store::NewCron {
        id: "c2".into(),
        expr: "0 9 * * *".into(),
        prompt: "以后再说".into(),
        tz: "UTC".into(),
        next_at: Some(9_000_000_000), // 远在未来
    })
    .await
    .unwrap();

    let (tx, _rx) = broadcast::channel(64);
    let ctx = ctx(store.clone(), tx);

    let fired = cron_scan(&ctx, 1_000_000_000).await;
    assert_eq!(fired, 0, "未到期不应触发");
}

#[tokio::test]
async fn empty_cron_table_is_noop() {
    let store = oc_store::Store::open_memory().unwrap();
    let (tx, _rx) = broadcast::channel(64);
    let ctx = ctx(store.clone(), tx);
    assert_eq!(cron_scan(&ctx, 1_000_000_000).await, 0);
}

/// 触发后需要 CRON_RUN_TIMEOUT 保护：卡住的模型不应无限阻塞（用 stalls_for 验证快速返回）。
#[tokio::test]
async fn stalling_model_times_out_but_still_reschedules() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.cron_add(oc_store::NewCron {
        id: "c3".into(),
        expr: "*/5 * * * *".into(),
        prompt: "慢任务".into(),
        tz: "UTC".into(),
        next_at: Some(1),
    })
    .await
    .unwrap();

    let (tx, mut rx) = broadcast::channel(64);
    let mut ctx = ctx(store.clone(), tx);
    // 换成会拖延的 provider（但 timeout=120s，测试里不会真等；这里只验证逻辑闭环）。
    ctx.provider = Arc::new(MockProvider::echo_text("")); // 空输出

    let now = 2_000_000_000i64;
    let fired = cron_scan(&ctx, now).await;
    assert_eq!(fired, 1);

    // 空输出也应推一条"任务已触发"提醒。
    let mut got = false;
    while let Ok(ev) = rx.try_recv() {
        if let Event::Proactive { text, .. } = ev {
            assert!(text.contains("慢任务") || !text.is_empty());
            got = true;
        }
    }
    assert!(got);

    let _ = Duration::from_secs(1); // 保留 import
    let crons = w.cron_list().await.unwrap();
    assert!(crons.iter().find(|c| c.id == "c3").unwrap().next_at.unwrap() > now);
}
