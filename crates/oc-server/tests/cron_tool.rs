//! P1-5：`cron` 工具的四个 op（add/delay/list/rm）经 proactive 落地。
//!
//! 覆盖真机暴露的三个缺陷：
//! 1. 表达式按 UTC 解释 → 东八区偏 8 小时（`add_*_timezone`）。
//! 2. 「N 秒后」无法表达，只能凑成每日重复任务（`delay_*`）。
//! 3. 模型没有查询/删除手段，用户说「没收到」时只能猜（`list_*` / `rm_*`）。

use std::sync::Arc;

use oc_llm::mock::MockProvider;
use oc_proto::Event;
use oc_server::proactive::{handle_cron_op, ProactiveCtx};
use oc_tools::types::CronOp;
use tokio::sync::broadcast;

/// 默认时区取东八，正是真机出错的那个时区。
fn ctx(store: oc_store::Store) -> (ProactiveCtx, broadcast::Receiver<Event>) {
    let (tx, rx) = broadcast::channel(64);
    (
        ProactiveCtx {
            provider: Arc::new(MockProvider::echo_text("提醒")),
            events: tx,
            store,
            model: "mock".into(),
            soul: "人格".into(),
            default_tz: "Asia/Shanghai".into(),
        },
        rx,
    )
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn add_creates_recurring_task_in_local_timezone() {
    let store = oc_store::Store::open_memory().unwrap();
    let (ctx, _rx) = ctx(store.clone());

    let out = handle_cron_op(
        &ctx,
        CronOp::Add {
            expr: "0 9 * * *".into(),
            prompt: "早安".into(),
            tz: None, // 省略 → 应用 default_tz，而非 UTC
        },
    )
    .await
    .expect("add 应成功");

    let rows = store.writer().cron_list().await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.tz, "Asia/Shanghai", "省略 tz 应落为本机时区，不是 UTC");

    // next_at 渲染成东八墙上时间必须是 09:00。
    let local = oc_core::proactive::fmt_in_tz(row.next_at.unwrap(), "Asia/Shanghai").unwrap();
    assert!(local.ends_with("09:00"), "应落在本地 09:00，实为 {local}");

    // 回执要含本地时间，供模型向用户复述（此前只有 unix 秒，模型无从发现算错）。
    assert!(out.contains("09:00"), "回执应含本地时间: {out}");
}

/// tz 拼错必须报错，不能静默按 UTC 落库——那正是「看着成功了但永不触发」的成因。
#[tokio::test]
async fn add_rejects_unknown_timezone() {
    let store = oc_store::Store::open_memory().unwrap();
    let (ctx, _rx) = ctx(store.clone());

    let err = handle_cron_op(
        &ctx,
        CronOp::Add {
            expr: "0 9 * * *".into(),
            prompt: "x".into(),
            tz: Some("Asia/Shanghia".into()), // 拼错
        },
    )
    .await
    .expect_err("未知时区应报错");
    assert!(err.contains("时区"), "错误应点明时区问题: {err}");
    assert!(
        store.writer().cron_list().await.unwrap().is_empty(),
        "校验失败不应落库"
    );
}

#[tokio::test]
async fn add_rejects_invalid_expression() {
    let store = oc_store::Store::open_memory().unwrap();
    let (ctx, _rx) = ctx(store.clone());

    let err = handle_cron_op(
        &ctx,
        CronOp::Add { expr: "99 * * * *".into(), prompt: "x".into(), tz: None },
    )
    .await
    .expect_err("非法表达式应报错");
    assert!(!err.is_empty());
    assert!(store.writer().cron_list().await.unwrap().is_empty());
}

/// 「N 秒后」应落成一次性任务，且 next_at 精确到秒——不能被舍进整分钟，
/// 也不能变成每天重复。
#[tokio::test]
async fn delay_creates_one_shot_with_second_precision() {
    let store = oc_store::Store::open_memory().unwrap();
    let (ctx, _rx) = ctx(store.clone());

    let before = now_secs();
    handle_cron_op(&ctx, CronOp::Delay { secs: 90, prompt: "看短信".into() })
        .await
        .expect("delay 应成功");

    let rows = store.writer().cron_list().await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(
        oc_core::proactive::is_once(&row.expr),
        "应标记为一次性任务，实为 `{}`",
        row.expr
    );

    // 90 秒后，允许测试执行本身的少量偏差。
    let delta = row.next_at.unwrap() - before;
    assert!(
        (88..=95).contains(&delta),
        "应约 90 秒后触发（秒级精度），实差 {delta} 秒"
    );
}

#[tokio::test]
async fn delay_rejects_nonpositive_and_overlong() {
    let store = oc_store::Store::open_memory().unwrap();
    let (ctx, _rx) = ctx(store.clone());

    assert!(handle_cron_op(&ctx, CronOp::Delay { secs: 0, prompt: "x".into() })
        .await
        .is_err());
    assert!(
        handle_cron_op(&ctx, CronOp::Delay { secs: 400 * 86_400, prompt: "x".into() })
            .await
            .is_err(),
        "超上限应报错并提示改用重复表达式"
    );
    assert!(store.writer().cron_list().await.unwrap().is_empty());
}

/// list 要给出模型能据以答复用户的信息：id、本地时间、剩余时长、内容。
#[tokio::test]
async fn list_reports_local_time_and_remaining() {
    let store = oc_store::Store::open_memory().unwrap();
    let (ctx, _rx) = ctx(store.clone());

    handle_cron_op(&ctx, CronOp::Delay { secs: 120, prompt: "喝水".into() })
        .await
        .unwrap();
    handle_cron_op(
        &ctx,
        CronOp::Add { expr: "0 9 * * *".into(), prompt: "早安".into(), tz: None },
    )
    .await
    .unwrap();

    let out = handle_cron_op(&ctx, CronOp::List).await.unwrap();
    assert!(out.contains("喝水") && out.contains("早安"), "应列出全部任务: {out}");
    assert!(out.contains("一次性"), "应区分一次性/重复: {out}");
    assert!(out.contains("分钟后") || out.contains("秒后"), "应给剩余时长: {out}");
    assert!(out.contains("Asia/Shanghai"), "应给时区: {out}");
}

#[tokio::test]
async fn list_on_empty_table_says_so() {
    let store = oc_store::Store::open_memory().unwrap();
    let (ctx, _rx) = ctx(store.clone());
    let out = handle_cron_op(&ctx, CronOp::List).await.unwrap();
    assert!(out.contains("没有"), "空表应明确说没有任务: {out}");
}

#[tokio::test]
async fn rm_deletes_task() {
    let store = oc_store::Store::open_memory().unwrap();
    let (ctx, _rx) = ctx(store.clone());

    handle_cron_op(&ctx, CronOp::Delay { secs: 300, prompt: "取消我".into() })
        .await
        .unwrap();
    let id = store.writer().cron_list().await.unwrap()[0].id.clone();

    let out = handle_cron_op(&ctx, CronOp::Rm { id: id.clone() }).await.unwrap();
    assert!(out.contains("已删除"), "回执应确认删除: {out}");
    assert!(store.writer().cron_list().await.unwrap().is_empty());
}

/// 删不存在的 id 不该报错，但必须**明说没删到**——否则模型会向用户确认"已取消"。
#[tokio::test]
async fn rm_missing_id_reports_not_found() {
    let store = oc_store::Store::open_memory().unwrap();
    let (ctx, _rx) = ctx(store.clone());

    let out = handle_cron_op(&ctx, CronOp::Rm { id: "cron-nope".into() })
        .await
        .unwrap();
    assert!(
        out.contains("没有"),
        "不存在的 id 应明确回「没有这条」，而非含糊的成功: {out}"
    );
}
