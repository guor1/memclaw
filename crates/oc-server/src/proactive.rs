//! 主动性调度（设计 §7.4、§12.2）。
//!
//! 心跳 tick 周期性触发 cron 扫描：到期项 → 起**隔离子会话**跑一轮 → 结果经
//! `Event::Proactive{Reminder}` 推给所有 client → 用 core::next_fire 重排下次触发。
//!
//! 判定纯在 core（next_fire / eval_due），起会话/推事件/读写在此。
//! **失败绝不阻塞主会话**：本模块只告警，不向上传播错误。

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_llm::{Delta, Message, ModelRequest, MsgRole, Provider};
use oc_proto::{Event, ProactiveKind, ProactiveSource, SessionId};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// 隔离子会话单轮的墙钟上限（防止 cron 轮卡住占资源）。
const CRON_RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// cron 调度所需的最小上下文（heartbeat 闭包持有其克隆）。
#[derive(Clone)]
pub struct ProactiveCtx {
    pub provider: Arc<dyn Provider>,
    pub events: broadcast::Sender<Event>,
    pub store: oc_store::Store,
    pub model: String,
    pub soul: String,
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
        // 起隔离子会话跑一轮（注入 cron.prompt）。
        let text = run_isolated_turn(ctx, &cron.prompt).await;

        // 结果推送（即使为空也推一条提醒，告知任务已触发）。
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

        // 用 core::next_fire 算下次触发并重排；失败则禁用式处理（next_at=None）。
        let next_at = oc_core::proactive::next_fire(&cron.expr, now_secs)
            .ok()
            .flatten();
        if let Err(e) = ctx
            .store
            .writer()
            .cron_mark_fired(cron.id.clone(), now_secs, next_at)
            .await
        {
            warn!(id = %cron.id, error = %e, "proactive：更新 cron 触发状态失败");
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
        fired += 1;
    }
    if fired > 0 {
        info!(fired, "proactive：本轮 cron 触发完成");
    }
    fired
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
