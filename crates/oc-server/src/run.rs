//! 单个 run 的驱动器（设计 §7.3）。
//!
//! 把 oc-core 的纯状态机与 oc-llm 的 Delta 流粘起来，并套三层防卡死（§10）：
//! - **空闲看门狗**：每个 Delta 之间 `timeout`，超时判模型不吐 token
//! - **run 超时**：整个 run 墙钟上限（0=无限）
//! - **abort**：CancellationToken，用户/看门狗/超时触发
//!
//! panic 隔离由调用方（session actor）用 catch_unwind 包裹。

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_core::agent::{step, Effect, RunOutcome, RunState, StepEvent};
use oc_llm::{Delta, FinishReason, Message, ModelRequest, MsgRole, Provider};
use oc_proto::{Event, LifecyclePhase, RunErrorKind, RunId};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// run 驱动所需的上下文。
pub struct RunCtx {
    pub run_id: RunId,
    pub user_text: String,
    pub system_prompt: Option<String>,
    pub model: String,
    pub provider: Arc<dyn Provider>,
    pub events: broadcast::Sender<Event>,
    pub cancel: CancellationToken,
    /// 空闲看门狗：两个 Delta 之间的最大间隔。
    pub idle_timeout: Duration,
    /// run 墙钟上限；None = 无限。
    pub run_timeout: Option<Duration>,
}

/// 驱动一个 run 到终态。返回归一化终态与累积的完整回复。
pub async fn drive(ctx: RunCtx) -> RunOutcome {
    let run_timeout = ctx.run_timeout;
    match run_timeout {
        Some(d) => match tokio::time::timeout(d, drive_inner(&ctx)).await {
            Ok(outcome) => outcome,
            Err(_) => {
                // run 超时：中止并归一。
                ctx.cancel.cancel();
                emit_error(&ctx, RunErrorKind::Timeout, "run 超时");
                RunOutcome::Aborted
            }
        },
        None => drive_inner(&ctx).await,
    }
}

async fn drive_inner(ctx: &RunCtx) -> RunOutcome {
    let mut state = RunState::Idle;
    let mut acc = String::new();

    // 启动步进。
    let (next, effects) = step(state, StepEvent::Start, &acc);
    state = next;
    if !execute_effects(ctx, &effects, &mut acc).await {
        return outcome_of(&state);
    }

    // 需要调用模型时进入流循环（M3 无工具，单次模型调用即完成）。
    if !effects.iter().any(|e| matches!(e, Effect::CallModel)) {
        return outcome_of(&state);
    }

    let req = ModelRequest {
        model: ctx.model.clone(),
        system: ctx.system_prompt.clone(),
        messages: vec![Message {
            role: MsgRole::User,
            content: ctx.user_text.clone(),
            tool_call_id: None,
        }],
        tools: vec![],
        max_tokens: None,
        temperature: None,
    };

    let mut stream = match ctx.provider.stream_chat(req, ctx.cancel.clone()).await {
        Ok(s) => s,
        Err(e) => {
            let (next, effs) = step(state, StepEvent::ModelError(e.to_string()), &acc);
            state = next;
            execute_effects(ctx, &effs, &mut acc).await;
            return outcome_of(&state);
        }
    };

    loop {
        // 空闲看门狗：等下一个 Delta，超时或取消则中止。
        let next_delta = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                let (n, effs) = step(state.clone(), StepEvent::Abort, &acc);
                state = n;
                execute_effects(ctx, &effs, &mut acc).await;
                return outcome_of(&state);
            }
            d = tokio::time::timeout(ctx.idle_timeout, stream.next()) => d,
        };

        let delta = match next_delta {
            Ok(Some(Ok(delta))) => delta,
            Ok(Some(Err(e))) => {
                let (n, effs) = step(state, StepEvent::ModelError(e.to_string()), &acc);
                state = n;
                execute_effects(ctx, &effs, &mut acc).await;
                return outcome_of(&state);
            }
            Ok(None) => {
                // 流结束但未见 Done：视为完成。
                let (n, effs) = step(state, StepEvent::ModelDone, &acc);
                state = n;
                execute_effects(ctx, &effs, &mut acc).await;
                return outcome_of(&state);
            }
            Err(_) => {
                // 空闲看门狗触发。
                warn!(run_id = %ctx.run_id, "空闲看门狗触发：模型停止吐 token");
                ctx.cancel.cancel();
                let (n, effs) = step(state, StepEvent::Abort, &acc);
                state = n;
                execute_effects(ctx, &effs, &mut acc).await;
                emit_error(ctx, RunErrorKind::Timeout, "空闲看门狗触发");
                return outcome_of(&state);
            }
        };

        let event = match delta {
            Delta::Text(t) => StepEvent::ModelText(t),
            Delta::ToolCall(tc) => StepEvent::ModelToolCall {
                call_id: tc.call_id,
                name: tc.name.unwrap_or_default(),
                args: tc.args_chunk,
            },
            Delta::Usage(_) => {
                debug!("usage delta（M5 计量）");
                continue;
            }
            Delta::Done(FinishReason::Length) => StepEvent::ModelLength,
            Delta::Done(_) => StepEvent::ModelDone,
        };

        let (next, effects) = step(state, event, &acc);
        state = next;
        if !execute_effects(ctx, &effects, &mut acc).await {
            return outcome_of(&state);
        }
        if matches!(state, RunState::Terminal(_)) {
            return outcome_of(&state);
        }
    }
}

/// 执行副作用。返回 false 表示已到终态无需继续。
async fn execute_effects(ctx: &RunCtx, effects: &[Effect], acc: &mut String) -> bool {
    let mut keep_going = true;
    for eff in effects {
        match eff {
            Effect::EmitLifecycleStart => {
                let _ = ctx.events.send(Event::Lifecycle {
                    run_id: ctx.run_id.clone(),
                    phase: LifecyclePhase::Start,
                });
            }
            Effect::EmitAssistant(text) => {
                acc.push_str(text);
                let _ = ctx.events.send(Event::Assistant {
                    run_id: ctx.run_id.clone(),
                    delta: text.clone(),
                });
            }
            Effect::EmitLifecycleEnd => {
                let _ = ctx.events.send(Event::Lifecycle {
                    run_id: ctx.run_id.clone(),
                    phase: LifecyclePhase::End,
                });
                keep_going = false;
            }
            Effect::EmitLifecycleError(msg) => {
                emit_error(ctx, RunErrorKind::Failed, msg);
                keep_going = false;
            }
            Effect::PersistAssistant(_full) => {
                // M3：暂不落库（store 写队列在 M4/M5 接入）。
                debug!(run_id = %ctx.run_id, "persist assistant（M5 落库）");
            }
            Effect::CallModel => { /* 由 drive_inner 主循环处理 */ }
            Effect::ExecTool { .. } => {
                // M4 接工具执行。
                debug!("exec tool（M4）");
            }
        }
    }
    keep_going
}

fn emit_error(ctx: &RunCtx, kind: RunErrorKind, msg: &str) {
    let _ = ctx.events.send(Event::Lifecycle {
        run_id: ctx.run_id.clone(),
        phase: LifecyclePhase::Error {
            message: msg.to_string(),
            kind,
        },
    });
}

fn outcome_of(state: &RunState) -> RunOutcome {
    match state {
        RunState::Terminal(o) => o.clone(),
        _ => RunOutcome::Failed("run 未到终态".into()),
    }
}
