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
    /// 持久化句柄（落库 assistant/tool 消息）。
    pub store: oc_store::Store,
    /// 预加载的对话历史（含本轮已落库的用户消息），作为初始 messages。
    pub history: Vec<Message>,
    /// SOUL.md 人格文本（每轮经 oc-core::prompt 确定性组装）。
    pub soul: String,
    /// 技能文档（~/.oc/skills/*.md），确定性排序注入 prompt。
    pub skills: Vec<oc_core::prompt::SkillBrief>,
    /// bootstrap 注入的 curated 记忆行（第 4/5 段填充；当前为空）。
    pub bootstrap: Vec<oc_core::prompt::MemLine>,
    /// 上下文压缩配置（设计 §4）。
    pub compact_cfg: oc_core::compaction::CompactCfg,
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
    // history 已含本轮落库的用户消息；为空时回退到 user_text。
    let mut messages: Vec<Message> = if ctx.history.is_empty() {
        vec![Message {
            role: MsgRole::User,
            content: ctx.user_text.clone(),
            tool_call_id: None,
            tool_calls: vec![],
        }]
    } else {
        ctx.history.clone()
    };
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
            TurnResult::Completed => {
                // 落库最终 assistant 回复（重启不失忆）。
                persist(ctx, oc_store::Role::Assistant, &acc).await;
                return outcome_of(&state);
            }
            TurnResult::Terminal(outcome) => return outcome,
            TurnResult::ToolCall { call_id, name, args } => {
                // 记录本轮 assistant（可能含文本）+ 本次工具调用到历史。
                // 必须携带 tool_calls：OpenAI 协议要求 tool 结果消息前有一条
                // 带匹配 tool_calls 的 assistant 消息，否则回喂时 400。
                messages.push(Message {
                    role: MsgRole::Assistant,
                    content: acc.clone(),
                    tool_call_id: None,
                    tool_calls: vec![oc_llm::ToolCallSpec {
                        id: call_id.clone(),
                        name: name.clone(),
                        args: args.clone(),
                    }],
                });
                if !acc.is_empty() {
                    persist(ctx, oc_store::Role::Assistant, &acc).await;
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
                persist(ctx, oc_store::Role::Tool, &output).await;
                messages.push(Message {
                    role: MsgRole::Tool,
                    content: output,
                    tool_call_id: Some(call_id.clone()),
                    tool_calls: vec![],
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
/// 按 oc-core 的压缩计划裁剪消息列表（设计 §4 compaction）。
///
/// 优先剪枝旧工具结果，其次丢弃最早消息；最近若干条始终保留。
fn apply_compaction(messages: &[Message], cfg: &oc_core::compaction::CompactCfg) -> Vec<Message> {
    use oc_core::compaction::{plan_compaction, MsgMeta};

    let metas: Vec<MsgMeta> = messages
        .iter()
        .enumerate()
        .map(|(i, m)| MsgMeta {
            index: i,
            tokens: (m.content.chars().count() as i64 / 4).max(1),
            is_tool_result: matches!(m.role, MsgRole::Tool),
        })
        .collect();

    let plan = plan_compaction(&metas, cfg);
    if plan.is_noop() {
        return messages.to_vec();
    }

    let prune_chars = (plan.prune_to_tokens * 4).max(0) as usize;
    let mut out = Vec::with_capacity(messages.len());
    for (i, m) in messages.iter().enumerate() {
        if plan.drop.contains(&i) {
            continue;
        }
        if plan.prune_tool_results.contains(&i) && m.content.chars().count() > prune_chars {
            let kept: String = m.content.chars().take(prune_chars).collect();
            out.push(Message {
                role: m.role,
                content: format!("{kept}\n…[旧工具输出已剪枝]"),
                tool_call_id: m.tool_call_id.clone(),
                tool_calls: m.tool_calls.clone(),
            });
        } else {
            out.push(m.clone());
        }
    }
    if !plan.drop.is_empty() {
        tracing::debug!(
            dropped = plan.drop.len(),
            pruned = plan.prune_tool_results.len(),
            "上下文压缩已应用"
        );
    }
    out
}

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
        // 经 oc-core::prompt 确定性组装（人格 + 工具 + 记忆 + 时间）。
        system: Some(render_prompt(ctx)),
        // 超预算时按 oc-core 压缩计划裁剪（剪枝旧工具结果 → 丢弃最早消息）。
        messages: apply_compaction(messages, &ctx.compact_cfg),
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

/// 用 oc-core::prompt 确定性组装系统提示词（设计 §4.4）。
///
/// 工具/技能/记忆按稳定 key 排序，时间放易变尾部，保证 prompt cache 前缀稳定。
fn render_prompt(ctx: &RunCtx) -> String {
    use oc_core::prompt::{render_system_prompt, PromptInputs, ToolBrief};

    let tools: Vec<ToolBrief> = ctx
        .tools
        .as_ref()
        .map(|t| {
            t.llm_specs()
                .into_iter()
                .map(|s| ToolBrief {
                    name: s.name,
                    description: s.description,
                })
                .collect()
        })
        .unwrap_or_default();

    let soul = if ctx.soul.trim().is_empty() {
        DEFAULT_SOUL
    } else {
        ctx.soul.as_str()
    };

    let now = now_rfc3339();
    let rendered = render_system_prompt(&PromptInputs {
        soul,
        platform: PLATFORM_HINT,
        bootstrap: &ctx.bootstrap,
        skills: &ctx.skills,
        tools: &tools,
        now: &now,
    });
    rendered.full()
}

/// 运行环境提示：告诉模型 exec 工具的目标 OS / shell，避免写错命令语法。
/// 编译期按目标平台确定。
#[cfg(windows)]
const PLATFORM_HINT: &str = "操作系统：Windows。exec 工具通过 `cmd.exe /C` 执行命令，\
请使用 Windows/cmd 命令语法，不要使用 Unix/bash 语法（例如取当前时间用 `date /T & time /T`，\
不要用 `date '+%Y-%m-%d'`）。";
#[cfg(not(windows))]
const PLATFORM_HINT: &str = "操作系统：类 Unix（Linux/macOS）。exec 工具通过 `sh -c` 执行命令，\
请使用 POSIX shell 语法。";

/// 缺少 SOUL.md 时的默认人格。
const DEFAULT_SOUL: &str = "你是 oc，一个长期陪伴用户的个人助手。\
说话简洁、直接、务实。你可以使用工具执行命令和读写文件来完成任务；\
危险操作会先请求用户审批。";

/// 当前时间（RFC3339 近似，避免额外依赖格式化）。
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 简易表示：unix 秒。人类可读格式化留待引入 time 格式化时再换。
    format!("unix:{secs}")
}

/// 落库一条消息（空文本跳过）。失败仅告警，不阻断 run。
async fn persist(ctx: &RunCtx, role: oc_store::Role, content: &str) {
    if content.is_empty() {
        return;
    }
    let est = (content.chars().count() as i64 / 4).max(1);
    if let Err(e) = ctx
        .store
        .writer()
        .append_entry(oc_store::NewEntry {
            session_id: "main".into(),
            role,
            content: content.to_string(),
            tokens_est: est,
        })
        .await
    {
        warn!(run_id = %ctx.run_id, error = %e, "落库消息失败");
    }
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

