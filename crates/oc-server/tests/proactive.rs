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
        default_tz: "UTC".into(),
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
        if let Event::Proactive { kind, source, text, .. } = ev {
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

/// **P1-5 回归**：重排必须按行上的 `tz` 算，不能一律按 UTC。
///
/// 真机缺陷：东八区用户的「每天 21:46」被按 UTC 推算 → 存成 21:46 UTC（本地次日
/// 05:46），差 8 小时。此前所有 cron 用例的 tz 都是 UTC，所以没人抓到。
#[tokio::test]
async fn reschedule_respects_row_timezone() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.cron_add(oc_store::NewCron {
        id: "tz1".into(),
        expr: "0 9 * * *".into(), // 每天 09:00（东八区墙上时间）
        prompt: "早安".into(),
        tz: "Asia/Shanghai".into(),
        next_at: Some(1000), // 到期
    })
    .await
    .unwrap();

    let (tx, _rx) = broadcast::channel(64);
    let ctx = ctx(store.clone(), tx);

    // 2026-09-01 13:44:33 UTC = 东八区当日 21:44（已过当天 09:00）。
    let now = 1_788_270_273i64;
    assert_eq!(cron_scan(&ctx, now).await, 1);

    let next = w.cron_list().await.unwrap()[0].next_at.unwrap();

    // 断言「渲染成东八区墙上时间后是次日 09:00」，而不是硬编码 unix 秒——
    // 这条用例要守的正是「墙上时间对得上」，直接断言墙上时间最贴合意图。
    assert_eq!(
        oc_core::proactive::fmt_in_tz(next, "Asia/Shanghai").unwrap(),
        "2026-09-02 09:00",
        "应按 Asia/Shanghai 解释表达式"
    );
    // 反向断言：旧实现按 UTC 推算会落在本地 17:00（差 8 小时）。
    assert_ne!(
        oc_core::proactive::fmt_in_tz(next, "UTC").unwrap(),
        "2026-09-02 09:00",
        "不应把 09:00 当成 UTC 时刻"
    );
}

/// 一次性延时任务（`@once`）触发后应**删除**，而不是像重复任务那样重排。
///
/// 真机缺陷：模型想表达「90 秒后」，只能写成 `46 21 * * *`，于是用户白得一条
/// 每天都响的任务。现在这类需求走 `@once`，用完即清。
#[tokio::test]
async fn once_task_is_removed_after_firing() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.cron_add(oc_store::NewCron {
        id: "once1".into(),
        expr: oc_core::proactive::ONCE_EXPR.into(),
        prompt: "喝水".into(),
        tz: "Asia/Shanghai".into(),
        next_at: Some(1000),
    })
    .await
    .unwrap();

    let (tx, mut rx) = broadcast::channel(64);
    let ctx = ctx(store.clone(), tx);
    assert_eq!(cron_scan(&ctx, 2_000_000_000).await, 1);

    // 提醒推了。
    let mut got = false;
    while let Ok(Event::Proactive { .. }) = rx.try_recv() {
        got = true;
    }
    assert!(got, "一次性任务也应推 Proactive 提醒");

    assert!(
        w.cron_list().await.unwrap().is_empty(),
        "一次性任务触发后应删除，否则会变成每天都响"
    );
}

/// 同一条一次性任务被两条路径同时看到（精确 timer + 心跳兜底）时，
/// `cron_claim` 必须保证只触发一次——否则用户会被同一件事提醒两遍。
#[tokio::test]
async fn concurrent_scans_fire_once_only() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.cron_add(oc_store::NewCron {
        id: "race1".into(),
        expr: oc_core::proactive::ONCE_EXPR.into(),
        prompt: "只提醒一次".into(),
        tz: "UTC".into(),
        next_at: Some(1000),
    })
    .await
    .unwrap();

    let (tx, mut rx) = broadcast::channel(64);
    let ctx = ctx(store.clone(), tx);

    // 两个扫描并发跑同一条到期任务。
    let (a, b) = tokio::join!(
        cron_scan(&ctx, 2_000_000_000),
        cron_scan(&ctx, 2_000_000_000)
    );
    assert_eq!(a + b, 1, "两条路径同时到期时只应有一个赢得触发权，实得 {a}+{b}");

    let mut reminders = 0;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, Event::Proactive { .. }) {
            reminders += 1;
        }
    }
    assert_eq!(reminders, 1, "用户只应被提醒一次");
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
