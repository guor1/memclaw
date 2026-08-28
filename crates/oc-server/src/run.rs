//! 单个 run 的驱动器（设计 §7.3）。
//!
//! 把 oc-core 的纯状态机与 oc-llm 的 Delta 流粘起来，并套防卡死（§10）：
//! - **空闲看门狗**：每个 Delta 之间 `timeout`
//! - **run 超时**：整个 run 墙钟上限（0=无限）
//! - **abort**：CancellationToken
//! - **loop detection**：重复工具调用达阈值 → 打转终态
//! - **工具执行循环**：模型请求工具 → 执行 → 结果回喂 → 再次调用模型
//!
//! panic 隔离由调用方（session actor）用 catch_unwind 包裹。

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_core::agent::{step, Effect, RunOutcome, RunState, StepEvent};
use oc_core::tool::{detect_loop, ToolFingerprint};
use oc_llm::{Delta, FinishReason, Message, ModelRequest, MsgRole, Provider};
use oc_proto::{Event, LifecyclePhase, RunErrorKind, RunId, ToolCallId, ToolPhase};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::tools_bridge::ToolExecutor;

/// loop detection 阈值：同一工具调用重复达此次数判打转。
const LOOP_REPEAT_THRESHOLD: usize = 3;
/// 单个 run 内最大工具轮数（兜底，防无限工具循环）。
const MAX_TOOL_ROUNDS: usize = 24;

/// run 驱动所需的上下文。
pub struct RunCtx {
    pub run_id: RunId,
    pub user_text: String,
    pub system_prompt: Option<String>,
    pub model: String,
    pub provider: Arc<dyn Provider>,
    pub events: broadcast::Sender<Event>,
    pub cancel: CancellationToken,
    pub idle_timeout: Duration,
    pub run_timeout: Option<Duration>,
    /// 工具执行器（None = 无工具，纯对话）。
    pub tools: Option<ToolExecutor>,
}

/// 驱动一个 run 到终态。
pub async fn drive(ctx: RunCtx) -> RunOutcome {
    match ctx.run_timeout {
        Some(d) => match tokio::time::timeout(d, drive_inner(&ctx)).await {
            Ok(outcome) => outcome,
            Err(_) => {
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
    // 对话历史（含用户输入、模型回复、工具结果），供多轮工具调用。
    let mut messages: Vec<Message> = vec![Message {
        role: MsgRole::User,
        content: ctx.user_text.clone(),
        tool_call_id: None,
    }];
    // loop detection：工具调用指纹历史。
    let mut fingerprints: Vec<ToolFingerprint> = Vec::new();
    let mut tool_rounds = 0usize;

    // 启动步进：发 lifecycle start + 首次 CallModel。
    let (next, effects) = step(state, StepEvent::Start, &acc);
    state = next;
    if !execute_effects(ctx, &effects, &mut acc).await {
        return outcome_of(&state);
    }

    // 主循环：每次做一轮模型调用；若产生工具调用则执行后继续。
    loop {
        // 一次模型流：消费到 ModelDone / ModelToolCall / 错误 / 中止。
        let turn = run_model_turn(ctx, &mut state, &mut acc, &messages).await;

        match turn {
            TurnResult::Completed => return outcome_of(&state),
            TurnResult::Terminal(outcome) => return outcome,
            TurnResult::ToolCall { call_id, name, args } => {
                // 记录本轮 assistant（可能含文本 + 工具调用）到历史。
                if !acc.is_empty() {
                    messages.push(Message {
                        role: MsgRole::Assistant,
                        content: acc.clone(),
                        tool_call_id: None,
                    });
                }

                // loop detection。
                fingerprints.push(ToolFingerprint {
                    name: name.clone(),
                    args: args.clone(),
                });
                if detect_loop(&fingerprints, LOOP_REPEAT_THRESHOLD) {
                    warn!(run_id = %ctx.run_id, "loop detection：工具重复调用，判打转");
                    emit_error(ctx, RunErrorKind::LoopDetected, "检测到工具调用打转");
                    return RunOutcome::LoopDetected;
                }
                tool_rounds += 1;
                if tool_rounds > MAX_TOOL_ROUNDS {
                    emit_error(ctx, RunErrorKind::LoopDetected, "超过最大工具轮数");
                    return RunOutcome::LoopDetected;
                }

                // 执行工具。
                let output = exec_tool(ctx, &call_id, &name, &args).await;

                // 结果回喂历史 → 状态机回 AwaitingModel。
                messages.push(Message {
                    role: MsgRole::Tool,
                    content: output,
                    tool_call_id: Some(call_id.clone()),
                });
                let (next, effs) = step(
                    state.clone(),
                    StepEvent::ToolResult { call_id, output: String::new() },
                    &acc,
                );
                state = next;
                execute_effects(ctx, &effs, &mut acc).await;
                acc.clear(); // 新一轮 assistant 从空开始累积。
            }
        }
    }
}

/// 一次模型流的结果。
enum TurnResult {
    /// 模型正常结束（无工具）。
    Completed,
    /// 模型请求工具调用。
    ToolCall { call_id: String, name: String, args: String },
    /// 直接终态（错误/中止/超时）。
    Terminal(RunOutcome),
}

/// 跑一次模型流，消费 Delta 直到该轮结束。
async fn run_model_turn(
    ctx: &RunCtx,
    state: &mut RunState,
    acc: &mut String,
    messages: &[Message],
) -> TurnResult {
    let tool_specs = ctx
        .tools
        .as_ref()
        .map(|t| t.llm_specs())
        .unwrap_or_default();

    let req = ModelRequest {
        model: ctx.model.clone(),
        system: ctx.system_prompt.clone(),
        messages: messages.to_vec(),
        tools: tool_specs,
        max_tokens: None,
        temperature: None,
    };

    let mut stream = match ctx.provider.stream_chat(req, ctx.cancel.clone()).await {
        Ok(s) => s,
        Err(e) => {
            let (n, effs) = step(state.clone(), StepEvent::ModelError(e.to_string()), acc);
            *state = n;
            execute_effects(ctx, &effs, acc).await;
            return TurnResult::Terminal(outcome_of(state));
        }
    };

    // 累积工具调用分片。
    let mut tc_id = String::new();
    let mut tc_name = String::new();
    let mut tc_args = String::new();

    loop {
        let next_delta = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                let (n, effs) = step(state.clone(), StepEvent::Abort, acc);
                *state = n;
                execute_effects(ctx, &effs, acc).await;
                return TurnResult::Terminal(outcome_of(state));
            }
            d = tokio::time::timeout(ctx.idle_timeout, stream.next()) => d,
        };

        let delta = match next_delta {
            Ok(Some(Ok(d))) => d,
            Ok(Some(Err(e))) => {
                let (n, effs) = step(state.clone(), StepEvent::ModelError(e.to_string()), acc);
                *state = n;
                execute_effects(ctx, &effs, acc).await;
                return TurnResult::Terminal(outcome_of(state));
            }
            Ok(None) => {
                // 流结束未见 Done：视为完成。
                let (n, effs) = step(state.clone(), StepEvent::ModelDone, acc);
                *state = n;
                execute_effects(ctx, &effs, acc).await;
                return TurnResult::Completed;
            }
            Err(_) => {
                warn!(run_id = %ctx.run_id, "空闲看门狗触发");
                ctx.cancel.cancel();
                let (n, effs) = step(state.clone(), StepEvent::Abort, acc);
                *state = n;
                execute_effects(ctx, &effs, acc).await;
                emit_error(ctx, RunErrorKind::Timeout, "空闲看门狗触发");
                return TurnResult::Terminal(outcome_of(state));
            }
        };

        match delta {
            Delta::Text(t) => {
                let (n, effs) = step(state.clone(), StepEvent::ModelText(t), acc);
                *state = n;
                execute_effects(ctx, &effs, acc).await;
            }
            Delta::ToolCall(tc) => {
                if !tc.call_id.is_empty() {
                    tc_id = tc.call_id;
                }
                if let Some(name) = tc.name {
                    tc_name = name;
                }
                tc_args.push_str(&tc.args_chunk);
            }
            Delta::Usage(_) => {}
            Delta::Done(FinishReason::ToolUse) => {
                // 有工具调用。
                let (n, _) = step(
                    state.clone(),
                    StepEvent::ModelToolCall {
                        call_id: tc_id.clone(),
                        name: tc_name.clone(),
                        args: tc_args.clone(),
                    },
                    acc,
                );
                *state = n;
                return TurnResult::ToolCall {
                    call_id: if tc_id.is_empty() { gen_call_id() } else { tc_id },
                    name: tc_name,
                    args: tc_args,
                };
            }
            Delta::Done(_) => {
                let (n, effs) = step(state.clone(), StepEvent::ModelDone, acc);
                *state = n;
                execute_effects(ctx, &effs, acc).await;
                return TurnResult::Completed;
            }
        }
    }
}

/// 执行一个工具调用，返回净化后的结果文本（回喂模型）。
async fn exec_tool(ctx: &RunCtx, call_id: &str, name: &str, args: &str) -> String {
    let cid = ToolCallId::new(call_id.to_string());
    let _ = ctx.events.send(Event::Tool {
        run_id: ctx.run_id.clone(),
        call_id: cid.clone(),
        phase: ToolPhase::Start {
            name: name.to_string(),
            args_preview: truncate(args, 200),
        },
    });

    let Some(executor) = &ctx.tools else {
        return format!("工具 {name} 不可用（无工具执行器）");
    };

    let (status, content) = executor
        .run(name, args, &ctx.run_id, &cid, ctx.cancel.clone(), &ctx.events)
        .await;

    let _ = ctx.events.send(Event::Tool {
        run_id: ctx.run_id.clone(),
        call_id: cid,
        phase: ToolPhase::End { status },
    });
    let _ = status;
    content
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
            Effect::PersistAssistant(_full) => { /* M5 落库 */ }
            Effect::CallModel => { /* 由主循环处理 */ }
            Effect::ExecTool { .. } => { /* 由主循环 exec_tool 处理 */ }
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
        _ => RunOutcome::Completed,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut n = max;
        while n > 0 && !s.is_char_boundary(n) {
            n -= 1;
        }
        format!("{}…", &s[..n])
    }
}

fn gen_call_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

