//! 主动性调度（设计 §7.4、§12.2）。
//!
//! 心跳 tick 周期性触发 cron 扫描：到期项 → 起**隔离子会话**跑一轮 → 结果经
//! `Event::Proactive{Reminder}` 推给所有 client → 用 core::next_fire 重排下次触发。
//!
//! 判定纯在 core（next_fire / eval_due），起会话/推事件/读写在此。
//! **失败绝不阻塞主会话**：本模块只告警，不向上传播错误。
//!
//! **P1-5**：处理模型的 `cron` 工具调用（add/delay/list/rm）。两点要留意：
//! - 表达式按行上的 `tz` 解释（此前一律按 UTC，东八区偏 8 小时，真机上定时提醒从不触发）。
//! - 一次性延时（`delay`）另起**精确 timer**，不等心跳——心跳粒度是分钟级，
//!   「10 秒后提醒」靠扫描会迟到近一分钟。心跳仍作为重启后的兜底路径。

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_llm::{Delta, Message, ModelRequest, MsgRole, Provider};
use oc_proto::{Event, ProactiveKind, ProactiveSource, SessionId};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// 隔离子会话单轮的墙钟上限（防止 cron 轮卡住占资源）。
const CRON_RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// 一次性延时的上限（30 天）。超过则劝模型用重复表达式，避免 timer 长期挂着。
const MAX_DELAY_SECS: i64 = 30 * 86_400;

/// 多久以内的一次性任务值得挂精确 timer。
///
/// 更远的交给心跳扫描就够了（用户不会在意 3 天后的提醒差几十秒），也免得
/// 进程里挂一堆睡很久的 task。
const PRECISE_TIMER_HORIZON_SECS: i64 = 3600;

/// cron 调度所需的最小上下文（heartbeat 闭包持有其克隆）。
#[derive(Clone)]
pub struct ProactiveCtx {
    pub provider: Arc<dyn Provider>,
    pub events: broadcast::Sender<Event>,
    pub store: oc_store::Store,
    pub model: String,
    pub soul: String,
    /// 未指定 tz 时用的默认时区（IANA 名，由 CLI 探测本机时区传入）。
    ///
    /// 模型经常省略 tz；缺省按 UTC 会让「每天 9 点」在东八区变成下午 5 点。
    pub default_tz: String,
}

/// 执行一轮 cron 扫描。返回本轮触发的任务数。
pub async fn cron_scan(ctx: &ProactiveCtx, now_secs: i64) -> usize {
    let crons = match ctx.store.writer().cron_list().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "proactive：读取 cron 失败，跳过本轮");
            return 0;
        }
    };
    if crons.is_empty() {
        return 0;
    }

    // core 到期筛选。
    let entries: Vec<(String, Option<i64>)> = crons
        .iter()
        .filter(|c| c.enabled)
        .map(|c| (c.id.clone(), c.next_at))
        .collect();
    let due = oc_core::proactive::eval_due(&entries, now_secs);
    if due.is_empty() {
        return 0;
    }

    let mut fired = 0usize;
    for id in due {
        let Some(cron) = crons.iter().find(|c| c.id == id) else {
            continue;
        };
        if fire_cron(ctx, cron, now_secs).await {
            fired += 1;
        }
    }
    if fired > 0 {
        info!(fired, "proactive：本轮 cron 触发完成");
    }
    fired
}

/// 触发一条 cron：跑一轮 → 推事件 → 重排（重复）或删除（一次性）。
///
/// 返回是否真的触发了（一次性任务被另一路径抢先时返回 false）。
///
/// **两类任务的收尾时机刻意相反**，各有其理由：
///
/// - **一次性**：先删后跑，`DELETE` 的 rows-affected 天然是原子认领。它有两条触发
///   路径（精确 timer + 心跳兜底），必须防重，否则用户被同一件事提醒两遍。
/// - **重复**：先跑后重排。这类只有心跳一条路径在动（`Heartbeat` 串行 await 每个
///   tick，不会自我并发），本就无竞争；而若也改成「先占位再跑」，进程在这中间被杀
///   就会把 `next_at` 永久留空——一条每日提醒**从此再不触发**，比重复提醒一次糟得多。
///   保持后置：崩了 `next_at` 仍是旧值（已过期），下次扫描重试。
async fn fire_cron(ctx: &ProactiveCtx, cron: &oc_store::CronRow, now_secs: i64) -> bool {
    let once = oc_core::proactive::is_once(&cron.expr);

    // 一次性任务：删除即认领。败者（rows-affected=0）说明别人已接手，直接退出。
    if once {
        match ctx.store.writer().cron_rm(cron.id.clone()).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(id = %cron.id, "proactive：一次性任务已被另一路径触发，跳过");
                return false;
            }
            Err(e) => {
                warn!(id = %cron.id, error = %e, "proactive：认领一次性任务失败，跳过");
                return false;
            }
        }
    }

    // 起隔离子会话跑一轮（注入 cron.prompt）。
    let text = run_isolated_turn(ctx, &cron.prompt).await;

    // 结果推送（即使为空也推一条提醒，告知任务已触发）。
    // 模型轮失败/超时也照推：宁可提醒得干巴巴，也不能到点没声音。
    let out = if text.trim().is_empty() {
        format!("定时任务已触发：{}", cron.prompt)
    } else {
        text
    };
    // cron 是隔离子会话，事件归属 main 让 client 能在主视图显示。
    let _ = ctx.events.send(Event::Proactive {
        session: SessionId::main(),
        kind: ProactiveKind::Reminder,
        text: out,
        source: ProactiveSource::Cron { cron_id: cron.id.clone() },
    });

    if !once {
        // 按**该行自己的 tz** 算下次触发并重排。
        // P1-5：此前这里漏传 tz、一律按 UTC 推算，东八区用户的「每天 21:46」
        // 被排到本地次日 05:46，当晚永不触发。
        let next_at = oc_core::proactive::next_fire(&cron.expr, now_secs, &cron.tz)
            .ok()
            .flatten();
        if next_at.is_none() {
            warn!(id = %cron.id, expr = %cron.expr, tz = %cron.tz,
                  "proactive：无法算出下次触发，该任务将不再触发");
        }
        if let Err(e) = ctx
            .store
            .writer()
            .cron_mark_fired(cron.id.clone(), now_secs, next_at)
            .await
        {
            warn!(id = %cron.id, error = %e, "proactive：更新 cron 触发状态失败");
        }
    }

    // 审计。
    if let Err(e) = ctx
        .store
        .writer()
        .write_audit("cron".into(), "fire".into(), Some(cron.id.clone()))
        .await
    {
        warn!(id = %cron.id, error = %e, "proactive：cron 审计失败");
    }
    true
}

/// 起一个隔离子会话跑单轮模型调用，收集文本。带超时与中止；失败返回空串。
async fn run_isolated_turn(ctx: &ProactiveCtx, prompt: &str) -> String {
    let cancel = CancellationToken::new();
    let req = ModelRequest {
        model: ctx.model.clone(),
        system: Some(if ctx.soul.trim().is_empty() {
            "你是 oc 的定时任务执行器，简洁地完成被要求的提醒/总结。".to_string()
        } else {
            ctx.soul.clone()
        }),
        messages: vec![Message {
            role: MsgRole::User,
            content: prompt.to_string(),
            tool_call_id: None,
            tool_calls: vec![],
            reasoning: None,
        }],
        tools: Vec::new(), // cron 轮不带工具（M6 简化；后续可放开）。
        max_tokens: None,
        temperature: None,
    };

    let run = async {
        let mut stream = match ctx.provider.stream_chat(req, cancel.clone()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "proactive：cron 模型调用失败");
                return String::new();
            }
        };
        let mut acc = String::new();
        while let Some(delta) = stream.next().await {
            match delta {
                Ok(Delta::Text(t)) => acc.push_str(&t),
                Ok(Delta::Done(_)) => break,
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "proactive：cron 流错误");
                    break;
                }
            }
        }
        acc
    };

    match tokio::time::timeout(CRON_RUN_TIMEOUT, run).await {
        Ok(text) => text,
        Err(_) => {
            cancel.cancel();
            warn!("proactive：cron 轮超时");
            String::new()
        }
    }
}

/// 处理模型的 `cron` 工具调用（P1-5）。
///
/// 与 `cron_scan` 分工：那边是心跳扫到期并触发，这边是模型主动增删查。
/// 调用方：tools_bridge 的 cron gate pump。
pub async fn handle_cron_op(ctx: &ProactiveCtx, op: oc_tools::types::CronOp) -> Result<String, String> {
    use oc_tools::types::CronOp;
    match op {
        CronOp::Add { expr, prompt, tz } => cron_add(ctx, expr, prompt, tz).await,
        CronOp::Delay { secs, prompt } => cron_delay(ctx, secs, prompt).await,
        CronOp::List => cron_render_list(ctx).await,
        CronOp::Rm { id } => cron_remove(ctx, id).await,
    }
}

/// 新建重复任务：按 tz 校验表达式 → 算首次触发 → 落库。
async fn cron_add(
    ctx: &ProactiveCtx,
    expr: String,
    prompt: String,
    tz: Option<String>,
) -> Result<String, String> {
    // 模型常省略 tz；缺省用本机时区而非 UTC（否则「每天 9 点」在东八区成下午 5 点）。
    let tz = tz
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| ctx.default_tz.clone());

    // 校验表达式 + 时区，并算首次触发（委托 core，非法就不落库）。
    let now = now_secs();
    let next_at = oc_core::proactive::next_fire(&expr, now, &tz)
        .map_err(|e| format!("{e}"))?
        .ok_or_else(|| format!("表达式 `{expr}` 在未来一年内没有匹配时刻"))?;

    let id = new_cron_id();
    ctx.store
        .writer()
        .cron_add(oc_store::NewCron {
            id: id.clone(),
            expr: expr.clone(),
            prompt,
            tz: tz.clone(),
            next_at: Some(next_at),
        })
        .await
        .map_err(|e| format!("写入 cron 失败: {e}"))?;
    audit(ctx, "cron_add", &id).await;

    info!(id = %id, expr = %expr, tz = %tz, next_at, "proactive：cron 已创建");
    Ok(format!(
        "已创建重复提醒（id={id}）：`{expr}` [{tz}]，下次触发 {}",
        fmt_local(next_at, &tz)
    ))
}

/// 一次性延时任务：`next_at` 直接存绝对秒（不经表达式），并挂精确 timer。
///
/// 这条路径存在的原因：cron 最小粒度是分钟，「10 秒后提醒我」根本没法用表达式表达；
/// 真机上模型只好把「90 秒后」写成 `46 21 * * *`，结果建出一条**每天**都响的任务。
async fn cron_delay(ctx: &ProactiveCtx, secs: i64, prompt: String) -> Result<String, String> {
    if secs <= 0 {
        return Err("延时必须为正整数秒".to_string());
    }
    if secs > MAX_DELAY_SECS {
        return Err(format!(
            "延时上限 {} 天；更久的请用重复表达式（op=add）",
            MAX_DELAY_SECS / 86_400
        ));
    }

    let fire_at = now_secs() + secs;
    let id = new_cron_id();
    ctx.store
        .writer()
        .cron_add(oc_store::NewCron {
            id: id.clone(),
            expr: oc_core::proactive::ONCE_EXPR.to_string(),
            prompt,
            tz: ctx.default_tz.clone(), // 一次性任务不按表达式推算，tz 仅供展示
            next_at: Some(fire_at),
        })
        .await
        .map_err(|e| format!("写入延时任务失败: {e}"))?;
    audit(ctx, "cron_delay", &id).await;

    // 近期任务挂精确 timer（秒级准）；更远的交给心跳扫描即可。
    if secs <= PRECISE_TIMER_HORIZON_SECS {
        spawn_precise_timer(ctx.clone(), id.clone(), secs);
    }

    info!(id = %id, secs, fire_at, "proactive：一次性延时任务已创建");
    Ok(format!(
        "已设置一次性提醒（id={id}）：{secs} 秒后，即 {}",
        fmt_local(fire_at, &ctx.default_tz)
    ))
}

/// 挂一个精确 timer：睡到点 → 重新读该行（可能已被删/已触发）→ 触发。
///
/// 与心跳扫描共存，靠 `cron_claim` 的原子认领去重（见 [`fire_cron`]）。
/// 进程退出即消失，重启后由心跳兜底捞回过期项——一次性提醒迟到几十秒可接受，
/// 换来无需在启动时重建 timer 的复杂度。
fn spawn_precise_timer(ctx: ProactiveCtx, id: String, secs: i64) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs.max(0) as u64)).await;
        // 重新读：用户可能已 rm，或心跳已抢先触发。
        let rows = match ctx.store.writer().cron_list().await {
            Ok(r) => r,
            Err(e) => {
                warn!(id = %id, error = %e, "proactive：timer 读取 cron 失败");
                return;
            }
        };
        let Some(row) = rows.into_iter().find(|c| c.id == id) else {
            debug!(id = %id, "proactive：一次性任务已不存在（已删或已触发），timer 退出");
            return;
        };
        if !row.enabled {
            return;
        }
        fire_cron(&ctx, &row, now_secs()).await;
    });
}

/// 渲染任务清单给模型看。
///
/// 模型此前没有查询手段：真机上它建完任务就失联，用户说「没收到」时它只能猜，
/// 猜成了「这个环境的 cron 触发消息不会送达」，然后自己改用不可靠的替代方案。
async fn cron_render_list(ctx: &ProactiveCtx) -> Result<String, String> {
    let rows = ctx
        .store
        .writer()
        .cron_list()
        .await
        .map_err(|e| format!("读取 cron 失败: {e}"))?;
    if rows.is_empty() {
        return Ok("当前没有定时任务。".to_string());
    }
    let now = now_secs();
    let mut out = String::from("当前定时任务：\n");
    for c in rows {
        let kind = if oc_core::proactive::is_once(&c.expr) {
            "一次性".to_string()
        } else {
            format!("重复 `{}`", c.expr)
        };
        let next = match c.next_at {
            Some(t) => format!("{}（{}）", fmt_local(t, &c.tz), fmt_remaining(t - now)),
            None => "已停止".to_string(),
        };
        out.push_str(&format!(
            "- id={} | {} [{}] | 下次：{} | 「{}」{}\n",
            c.id,
            kind,
            c.tz,
            next,
            c.prompt,
            if c.enabled { "" } else { " [已停用]" }
        ));
    }
    Ok(out)
}

async fn cron_remove(ctx: &ProactiveCtx, id: String) -> Result<String, String> {
    let removed = ctx
        .store
        .writer()
        .cron_rm(id.clone())
        .await
        .map_err(|e| format!("删除 cron 失败: {e}"))?;
    if !removed {
        // 明确回「没这条」，免得模型以为删成功了还向用户确认。
        return Ok(format!("没有 id={id} 的定时任务（可能已触发并自动清理）。"));
    }
    audit(ctx, "cron_rm", &id).await;
    info!(id = %id, "proactive：cron 已删除");
    Ok(format!("已删除定时任务 id={id}。"))
}

fn new_cron_id() -> String {
    format!("cron-{}", uuid::Uuid::now_v7())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn audit(ctx: &ProactiveCtx, action: &str, id: &str) {
    if let Err(e) = ctx
        .store
        .writer()
        .write_audit("agent".into(), action.into(), Some(id.to_string()))
        .await
    {
        warn!(id = %id, action, error = %e, "proactive：审计失败");
    }
}

/// 把 unix 秒渲染成给定时区的墙上时间，供回执文本用。
///
/// 回执必须是**本地时间**：真机上模型只能看到 unix 秒，没法向用户复述「几点触发」，
/// 也就无从发现自己算错了时区。
fn fmt_local(ts: i64, tz: &str) -> String {
    oc_core::proactive::fmt_in_tz(ts, tz)
        .unwrap_or_else(|| format!("unix {ts}"))
}

fn fmt_remaining(delta: i64) -> String {
    if delta < 0 {
        return "已过期，待下次扫描触发".to_string();
    }
    match delta {
        0..=90 => format!("{delta} 秒后"),
        91..=5400 => format!("约 {} 分钟后", (delta + 30) / 60),
        _ => format!("约 {} 小时后", (delta + 1800) / 3600),
    }
}
