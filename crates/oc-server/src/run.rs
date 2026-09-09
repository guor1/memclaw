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
use oc_proto::{Event, LifecyclePhase, RunErrorKind, RunId, SessionId, ToolCallId, ToolPhase};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::sink::RunSink;
use crate::tools_bridge::ToolExecutor;

/// loop detection 阈值：同一工具调用重复达此次数判打转。
const LOOP_REPEAT_THRESHOLD: usize = 3;
/// 单个 run 内最大工具轮数（兜底，防无限工具循环）。
const MAX_TOOL_ROUNDS: usize = 24;

/// run 驱动所需的上下文。
pub struct RunCtx {
    /// 本 run 所属会话（事件归属 + transcript 落库）。
    pub session_id: SessionId,
    pub run_id: RunId,
    pub user_text: String,
    pub system_prompt: Option<String>,
    pub model: String,
    pub provider: Arc<dyn Provider>,
    /// run 内联事件出口（Assistant/Lifecycle/Tool/Approval）——有界背压，不丢（P0-1）。
    pub sink: RunSink,
    /// 广播总线：仅 Usage（低频、有独立订阅者更新 state）经此推送。
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
    /// 模型上下文窗口（token），随 Usage 事件推给 client。
    pub context_window: u32,
    /// 本机时区（IANA 名）：系统提示词里的「当前时间」按此渲染成本地墙上时间。
    pub default_tz: String,
    /// 运行时诊断句柄：run 在阶段迁移点更新（`oc debug` 采样）。
    pub diag: crate::diag::SessionDiag,
}

/// 驱动一个 run 到终态。
pub async fn drive(ctx: RunCtx) -> RunOutcome {
    match ctx.run_timeout {
        Some(d) => match tokio::time::timeout(d, drive_inner(&ctx)).await {
            Ok(outcome) => outcome,
            Err(_) => {
                ctx.cancel.cancel();
                emit_error(&ctx, RunErrorKind::Timeout, "run 超时").await;
                RunOutcome::Aborted
            }
        },
        None => drive_inner(&ctx).await,
    }
}

async fn drive_inner(ctx: &RunCtx) -> RunOutcome {
    let mut state = RunState::Idle;
    let mut acc = String::new();
    // thinking 模式的推理内容累积（每轮清空）；发起工具调用时随 assistant 消息回喂。
    let mut reasoning = String::new();
    // 对话历史（含用户输入、模型回复、工具结果），供多轮工具调用。
    // history 已含本轮落库的用户消息；为空时回退到 user_text。
    let mut messages: Vec<Message> = if ctx.history.is_empty() {
        vec![Message {
            role: MsgRole::User,
            content: ctx.user_text.clone(),
            tool_call_id: None,
            tool_calls: vec![],
            reasoning: None,
        }]
    } else {
        ctx.history.clone()
    };
    // loop detection：工具调用指纹历史。
    let mut fingerprints: Vec<ToolFingerprint> = Vec::new();
    let mut tool_rounds = 0usize;
    // 最近一次 provider 报告的真实输入 token 数（compaction 优先用它校准估算）。
    let mut last_input_tokens: Option<i64> = None;

    // 启动步进：发 lifecycle start + 首次 CallModel。
    let (next, effects) = step(state, StepEvent::Start, &acc);
    state = next;
    if !execute_effects(ctx, &effects, &mut acc).await {
        return outcome_of(&state);
    }

    // 主循环：每次做一轮模型调用；若产生工具调用则执行后继续。
    loop {
        // 一次模型流：消费到 ModelDone / ModelToolCall / 错误 / 中止。
        reasoning.clear();
        let turn = run_model_turn(ctx, &mut state, &mut acc, &mut reasoning, &mut last_input_tokens, &messages).await;

        match turn {
            TurnResult::Completed => {
                // 落库最终 assistant 回复（重启不失忆）。
                persist(ctx, oc_store::Role::Assistant, &acc, None, None).await;
                return outcome_of(&state);
            }
            TurnResult::Terminal(outcome) => return outcome,
            TurnResult::ToolCall { call_id, name, args } => {
                // 记录本轮 assistant（可能含文本）+ 本次工具调用到历史。
                // 必须携带 tool_calls：OpenAI 协议要求 tool 结果消息前有一条
                // 带匹配 tool_calls 的 assistant 消息，否则回喂时 400。
                let specs = vec![oc_llm::ToolCallSpec {
                    id: call_id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                }];
                messages.push(Message {
                    role: MsgRole::Assistant,
                    content: acc.clone(),
                    tool_call_id: None,
                    tool_calls: specs.clone(),
                    // thinking 模式：带回本轮 reasoning_content，否则回喂 400。
                    reasoning: if reasoning.is_empty() {
                        None
                    } else {
                        Some(reasoning.clone())
                    },
                });
                // 带 tool_calls 落库：`acc` 为空也要落，否则纯工具调用轮在历史里
                // 彻底消失，重放时又退回「只有宣告、没有调用」的坏样例（P2-4）。
                let tc_json = serde_json::to_string(&specs).ok();
                persist(ctx, oc_store::Role::Assistant, &acc, tc_json, None).await;

                // loop detection。
                fingerprints.push(ToolFingerprint {
                    name: name.clone(),
                    args: args.clone(),
                });
                if detect_loop(&fingerprints, LOOP_REPEAT_THRESHOLD) {
                    warn!(run_id = %ctx.run_id, "loop detection：工具重复调用，判打转");
                    emit_error(ctx, RunErrorKind::LoopDetected, "检测到工具调用打转").await;
                    return RunOutcome::LoopDetected;
                }
                tool_rounds += 1;
                ctx.diag.set_tool_rounds(tool_rounds);
                if tool_rounds > MAX_TOOL_ROUNDS {
                    emit_error(ctx, RunErrorKind::LoopDetected, "超过最大工具轮数").await;
                    return RunOutcome::LoopDetected;
                }

                // 执行工具。
                ctx.diag.set_phase(oc_proto::RunPhase::ToolExec);
                tracing::debug!(tool = %name, "执行工具");
                let output = exec_tool(ctx, &call_id, &name, &args).await;

                // 结果回喂历史 → 状态机回 AwaitingModel。
                persist(ctx, oc_store::Role::Tool, &output, None, Some(call_id.clone())).await;
                messages.push(Message {
                    role: MsgRole::Tool,
                    content: output,
                    tool_call_id: Some(call_id.clone()),
                    tool_calls: vec![],
                    reasoning: None,
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
fn apply_compaction(
    messages: &[Message],
    cfg: &oc_core::compaction::CompactCfg,
    real_input_tokens: Option<i64>,
) -> Vec<Message> {
    use oc_core::compaction::{plan_compaction, MsgMeta};

    // 每消息基础估算：字符/4。
    let est: Vec<i64> = messages
        .iter()
        .map(|m| (m.content.chars().count() as i64 / 4).max(1))
        .collect();
    let est_total: i64 = est.iter().sum();

    // 校准：若有 provider 报告的真实输入 token，用它按比例缩放每条估算，
    // 使总量贴近真实值（估算只反映字符数，真实值含分词/角色/工具结构开销）。
    // 真实值来自「上一轮」的完整 prompt，作为当前决策的近似基线。
    let scale = match real_input_tokens {
        Some(real) if est_total > 0 && real > 0 => real as f64 / est_total as f64,
        _ => 1.0,
    };

    let metas: Vec<MsgMeta> = messages
        .iter()
        .enumerate()
        .map(|(i, m)| MsgMeta {
            index: i,
            tokens: ((est[i] as f64 * scale).round() as i64).max(1),
            is_tool_result: matches!(m.role, MsgRole::Tool),
            // 发起过工具调用的 assistant：丢弃时必须连带它的结果（成对进出）。
            is_tool_dispatch: matches!(m.role, MsgRole::Assistant) && !m.tool_calls.is_empty(),
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
                reasoning: m.reasoning.clone(),
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
    reasoning: &mut String,
    last_input_tokens: &mut Option<i64>,
    messages: &[Message],
) -> TurnResult {
    let tool_specs = ctx
        .tools
        .as_ref()
        .map(|t| t.llm_specs())
        .unwrap_or_default();

    let compacted = apply_compaction(messages, &ctx.compact_cfg, *last_input_tokens);
    // role 序列 + 每条长度：排查「某种历史序列触发 provider 空回复」时用，
    // 不打全文（噪音 + 隐私）。空 content 的 assistant/tool 会显式标 empty。
    let shape: Vec<String> = compacted
        .iter()
        .map(|m| {
            let role = match m.role {
                MsgRole::System => "sys",
                MsgRole::User => "usr",
                MsgRole::Assistant => "ast",
                MsgRole::Tool => "tool",
            };
            let n = m.content.chars().count();
            let tc = if m.tool_calls.is_empty() { "" } else { "+tc" };
            format!("{role}{tc}:{n}")
        })
        .collect();
    tracing::debug!(
        msg_count = compacted.len(),
        orig_count = messages.len(),
        tools = tool_specs.len(),
        shape = %shape.join(","),
        "构建模型请求"
    );
    let req = ModelRequest {
        model: ctx.model.clone(),
        // 经 oc-core::prompt 确定性组装（人格 + 工具 + 记忆 + 时间）。
        system: Some(render_prompt(ctx)),
        // 超预算时按 oc-core 压缩计划裁剪（剪枝旧工具结果 → 丢弃最早消息）。
        messages: compacted,
        tools: tool_specs,
        max_tokens: None,
        temperature: None,
    };

    // 阶段：等待模型响应（诊断可见「卡在等模型」）。
    //
    // 这一段（建流 + 等首个 delta）是 run 的**静默期**：没有任何事件外发，
    // 所以 `emit_inline` 的「send 失败 → 断连」探测在这里根本不会被调用。
    // 客户端此时掉线，车道会一直占到空闲看门狗超时（生产默认 120s）。
    // 故两处等待都叠 `sink.closed()`——与 ask_user / 审批等待同一套机制。
    ctx.diag.set_phase(oc_proto::RunPhase::AwaitingModel);
    let t_open = std::time::Instant::now();
    let opened = tokio::select! {
        _ = ctx.sink.closed() => {
            warn!(run_id = %ctx.run_id, "建流期间客户端断开，收敛 run");
            ctx.cancel.cancel();
            let (n, effs) = step(state.clone(), StepEvent::Abort, acc);
            *state = n;
            execute_effects(ctx, &effs, acc).await;
            return TurnResult::Terminal(outcome_of(state));
        }
        r = ctx.provider.stream_chat(req, ctx.cancel.clone()) => r,
    };
    let mut stream = match opened {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "建流失败");
            ctx.diag.set_error(format!("建流失败: {e}"));
            let (n, effs) = step(state.clone(), StepEvent::ModelError(e.to_string()), acc);
            *state = n;
            execute_effects(ctx, &effs, acc).await;
            return TurnResult::Terminal(outcome_of(state));
        }
    };
    tracing::debug!(ms = t_open.elapsed().as_millis(), "模型流已建立");

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
            // 首个 delta 到来前这里是静默的（见上方说明）；delta 之间的间隙同理。
            _ = ctx.sink.closed() => {
                warn!(run_id = %ctx.run_id, acc_chars = acc.chars().count(), "等模型期间客户端断开，收敛 run");
                ctx.cancel.cancel();
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
                tracing::debug!(acc_chars = acc.chars().count(), "模型流结束（无显式 Done）");
                let (n, effs) = step(state.clone(), StepEvent::ModelDone, acc);
                *state = n;
                execute_effects(ctx, &effs, acc).await;
                return TurnResult::Completed;
            }
            Err(_) => {
                warn!(run_id = %ctx.run_id, acc_chars = acc.chars().count(), "空闲看门狗触发（模型迟迟无 delta）");
                ctx.diag.set_error("空闲看门狗触发".to_string());
                ctx.cancel.cancel();
                let (n, effs) = step(state.clone(), StepEvent::Abort, acc);
                *state = n;
                execute_effects(ctx, &effs, acc).await;
                emit_error(ctx, RunErrorKind::Timeout, "空闲看门狗触发").await;
                return TurnResult::Terminal(outcome_of(state));
            }
        };

        match delta {
            Delta::Text(t) => {
                let (n, effs) = step(state.clone(), StepEvent::ModelText(t), acc);
                *state = n;
                execute_effects(ctx, &effs, acc).await;
                // 诊断：刷新 last_delta_at + 累计文本长度（phase→Streaming）。
                ctx.diag.delta_seen(acc.chars().count());
            }
            // thinking 内容：仅累积（供工具调用轮回喂），不进 assistant 可见文本、
            // 不落库、不推事件流。
            Delta::Reasoning(r) => {
                reasoning.push_str(&r);
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
            Delta::Usage(u) => {
                // 记录真实输入 token（含本轮完整 prompt），供下一轮 compaction 校准。
                if u.input_tokens > 0 {
                    *last_input_tokens = Some(u.input_tokens as i64);
                    // 推 Usage 事件：client 实时显示「已用/窗口」。
                    let _ = ctx.events.send(Event::Usage {
                        session: ctx.session_id.clone(),
                        input_tokens: u.input_tokens,
                        context_window: ctx.context_window,
                    });
                }
            }
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
            Delta::Done(reason) => {
                tracing::debug!(?reason, acc_chars = acc.chars().count(), "模型轮结束");
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
    emit_inline(ctx, Event::Tool {
        session: ctx.session_id.clone(),
        run_id: ctx.run_id.clone(),
        call_id: cid.clone(),
        phase: ToolPhase::Start {
            name: name.to_string(),
            args_preview: truncate(args, 200),
        },
    }).await;

    let Some(executor) = &ctx.tools else {
        return format!("工具 {name} 不可用（无工具执行器）");
    };

    // 工具执行期间的 tool(update)/approval 事件也经 sink（保证与文本同序）。
    let (status, content) = executor
        .run(name, args, &ctx.session_id, &ctx.run_id, &cid, ctx.cancel.clone(), &ctx.sink)
        .await;

    emit_inline(ctx, Event::Tool {
        session: ctx.session_id.clone(),
        run_id: ctx.run_id.clone(),
        call_id: cid,
        phase: ToolPhase::End { status },
    }).await;
    let _ = status;
    content
}

/// 执行副作用。返回 false 表示已到终态无需继续。
///
/// 内联事件走 `ctx.sink`（背压，不丢）。sink 报下游不可达（连接断）时，
/// 除停止继续外还触发 `cancel`，让 run 尽快收敛、释放车道（不被死连接拖住）。
async fn execute_effects(ctx: &RunCtx, effects: &[Effect], acc: &mut String) -> bool {
    let mut keep_going = true;
    for eff in effects {
        match eff {
            Effect::EmitLifecycleStart => {
                if !emit_inline(ctx, Event::Lifecycle {
                    session: ctx.session_id.clone(),
                    run_id: ctx.run_id.clone(),
                    phase: LifecyclePhase::Start,
                }).await {
                    keep_going = false;
                }
            }
            Effect::EmitAssistant(text) => {
                acc.push_str(text);
                if !emit_inline(ctx, Event::Assistant {
                    session: ctx.session_id.clone(),
                    run_id: ctx.run_id.clone(),
                    delta: text.clone(),
                }).await {
                    keep_going = false;
                }
            }
            Effect::EmitLifecycleEnd => {
                emit_inline(ctx, Event::Lifecycle {
                    session: ctx.session_id.clone(),
                    run_id: ctx.run_id.clone(),
                    phase: LifecyclePhase::End,
                }).await;
                keep_going = false;
            }
            Effect::EmitLifecycleError(msg) => {
                emit_error(ctx, RunErrorKind::Failed, msg).await;
                keep_going = false;
            }
            Effect::PersistAssistant(_full) => { /* M5 落库 */ }
            Effect::CallModel => { /* 由主循环处理 */ }
            Effect::ExecTool { .. } => { /* 由主循环 exec_tool 处理 */ }
        }
    }
    keep_going
}

/// 发一个内联事件到 sink。返回 `false` 表示下游断连——同时触发 cancel 收敛 run。
async fn emit_inline(ctx: &RunCtx, ev: Event) -> bool {
    if ctx.sink.send(ev).await {
        true
    } else {
        // 连接已断：中止 run，避免后续 send 继续挂起、车道被锁。
        ctx.cancel.cancel();
        false
    }
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

    let now = now_local(&ctx.default_tz);
    let rendered = render_system_prompt(&PromptInputs {
        soul,
        platform: PLATFORM_HINT,
        // 模型身份取自进程内实际生效的值，不读配置文件：热改配置后文件与进程内可能
        // 不一致，而这里的 model 就是发进请求体的那个串、endpoint 就是拼进 URL 的那个。
        model: &ctx.model,
        provider: ctx.provider.id(),
        endpoint: ctx.provider.endpoint(),
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
/// 系统提示词里的「当前时间」：本地墙上时间 + 星期 + 时区名。
///
/// 曾经这里给的是 `unix:1788310800`。模型读不出「现在几点」，于是每设一次提醒都要
/// 先调 `sys:now`、再自己心算换算——多这一轮工具调用，就多一次撞上 provider
/// 「宣布调工具却不发」抖动的机会（真机上连续三轮因此失败）。
///
/// 时区无法解析时退回 unix 秒：宁可难读，也不给一个偏移错误的时间。
fn now_local(tz: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    oc_core::proactive::fmt_now_local(secs, tz).unwrap_or_else(|| format!("unix:{secs}"))
}

/// 落库一条消息。失败仅告警，不阻断 run。
///
/// 空文本**不一定**跳过：纯工具调用轮（模型一个字不说直接调工具）的 `content`
/// 就是空的，但它带 `tool_calls`——丢了这条，历史里就缺了「assistant 发起调用」
/// 的那一半，重放出来的序列既不合协议，也教不会模型该调工具（P2-4）。
async fn persist(
    ctx: &RunCtx,
    role: oc_store::Role,
    content: &str,
    tool_calls: Option<String>,
    tool_call_id: Option<String>,
) {
    if content.is_empty() && tool_calls.is_none() {
        return;
    }
    let est = (content.chars().count() as i64 / 4).max(1);
    if let Err(e) = ctx
        .store
        .writer()
        .append_entry(oc_store::NewEntry {
            session_id: ctx.session_id.to_string(),
            role,
            content: content.to_string(),
            tokens_est: est,
            tool_calls,
            tool_call_id,
        })
        .await
    {
        warn!(run_id = %ctx.run_id, error = %e, "落库消息失败");
    }
}

async fn emit_error(ctx: &RunCtx, kind: RunErrorKind, msg: &str) {
    emit_inline(ctx, Event::Lifecycle {
        session: ctx.session_id.clone(),
        run_id: ctx.run_id.clone(),
        phase: LifecyclePhase::Error {
            message: msg.to_string(),
            kind,
        },
    }).await;
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

