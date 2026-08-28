//! Agent 循环状态机（设计 §4.2）。纯状态机。
//!
//! `step(state, event) -> (next_state, effects)`：不执行副作用，只返回"要做什么"。
//! server 把 oc-llm 的 Delta 翻译成 [`StepEvent`] 喂进来，并执行返回的 [`Effect`]。
//! 这样 core 保持对 oc-llm / tokio 零依赖，可确定性单测。

/// run 的状态。
#[derive(Debug, Clone, PartialEq)]
pub enum RunState {
    /// 尚未开始。
    Idle,
    /// 已请求模型，等待流开始/增量。
    AwaitingModel,
    /// 正在接收 assistant 文本增量。
    Streaming,
    /// 请求了工具调用，等待结果（M4 接工具；M3 不产生）。
    AwaitingTool,
    /// 终态。
    Terminal(RunOutcome),
}

/// run 的归一化终态（设计 §4.2 终态归一化）。
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    /// 正常结束（模型无更多工具调用）。
    Completed,
    /// 用户/看门狗/超时中止。
    Aborted,
    /// 失败。
    Failed(String),
    /// panic 被 run 边界 catch_unwind 归一。
    Panicked,
    /// 模型打转（M4 loop detection）。
    LoopDetected,
}

/// 驱动状态机的步进事件（server 从各来源翻译而来）。
#[derive(Debug, Clone)]
pub enum StepEvent {
    /// 用户发起本轮。
    Start,
    /// 模型文本增量。
    ModelText(String),
    /// 模型请求工具调用（M4 起处理）。
    ModelToolCall { call_id: String, name: String, args: String },
    /// 模型正常结束。
    ModelDone,
    /// 模型达到长度上限。
    ModelLength,
    /// 传输错误。
    ModelError(String),
    /// 中止信号（用户 / 看门狗 / 超时）。
    Abort,
    /// 工具执行完成（M4）。
    ToolResult { call_id: String, output: String },
}

/// 状态机产出的副作用请求（由 server 执行）。
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// 向模型发起请求。
    CallModel,
    /// 广播 assistant 文本增量。
    EmitAssistant(String),
    /// 请求执行工具（M4）。
    ExecTool { call_id: String, name: String, args: String },
    /// 广播生命周期开始。
    EmitLifecycleStart,
    /// 广播生命周期结束。
    EmitLifecycleEnd,
    /// 广播生命周期错误。
    EmitLifecycleError(String),
    /// 持久化一条 assistant 消息（累积的完整文本）。
    PersistAssistant(String),
}

/// 纯步进。返回下一状态与要执行的副作用序列。
///
/// `acc` 是本轮已累积的 assistant 文本（server 维护，随 ModelText 增长），
/// 用于在结束时持久化完整回复。
pub fn step(state: RunState, event: StepEvent, acc: &str) -> (RunState, Vec<Effect>) {
    use Effect::*;
    use RunState::*;
    use StepEvent as E;

    match (&state, event) {
        // 启动：发 start 事件 + 调模型。
        (Idle, E::Start) => (AwaitingModel, vec![EmitLifecycleStart, CallModel]),

        // 收到首个/后续文本增量。
        (AwaitingModel, E::ModelText(t)) | (Streaming, E::ModelText(t)) => {
            (Streaming, vec![EmitAssistant(t)])
        }

        // 模型结束（M3：无工具，直接完成）。
        (AwaitingModel, E::ModelDone) | (Streaming, E::ModelDone) => (
            Terminal(RunOutcome::Completed),
            vec![PersistAssistant(acc.to_string()), EmitLifecycleEnd],
        ),

        // 长度上限也视为完成（保留已生成内容）。
        (AwaitingModel, E::ModelLength) | (Streaming, E::ModelLength) => (
            Terminal(RunOutcome::Completed),
            vec![PersistAssistant(acc.to_string()), EmitLifecycleEnd],
        ),

        // 工具调用（M4 起真正执行；M3 provider 不产生此事件）。
        (AwaitingModel, E::ModelToolCall { call_id, name, args })
        | (Streaming, E::ModelToolCall { call_id, name, args }) => (
            AwaitingTool,
            vec![ExecTool { call_id, name, args }],
        ),

        // 工具结果回来 → 再次调模型。
        (AwaitingTool, E::ToolResult { .. }) => (AwaitingModel, vec![CallModel]),

        // 中止：任何非终态 → Aborted。
        (s, E::Abort) if !matches!(s, Terminal(_)) => (
            Terminal(RunOutcome::Aborted),
            vec![EmitLifecycleError("aborted".into())],
        ),

        // 传输错误：任何非终态 → Failed。
        (s, E::ModelError(msg)) if !matches!(s, Terminal(_)) => (
            Terminal(RunOutcome::Failed(msg.clone())),
            vec![EmitLifecycleError(msg)],
        ),

        // 其它：无效转移，保持状态不产生副作用（防御性）。
        (s, _) => (s.clone(), vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_completes() {
        let (s, eff) = step(RunState::Idle, StepEvent::Start, "");
        assert_eq!(s, RunState::AwaitingModel);
        assert_eq!(eff, vec![Effect::EmitLifecycleStart, Effect::CallModel]);

        let (s, eff) = step(s, StepEvent::ModelText("你".into()), "");
        assert_eq!(s, RunState::Streaming);
        assert_eq!(eff, vec![Effect::EmitAssistant("你".into())]);

        let (s, eff) = step(s, StepEvent::ModelText("好".into()), "你");
        assert_eq!(s, RunState::Streaming);
        assert_eq!(eff, vec![Effect::EmitAssistant("好".into())]);

        let (s, eff) = step(s, StepEvent::ModelDone, "你好");
        assert_eq!(s, RunState::Terminal(RunOutcome::Completed));
        assert_eq!(
            eff,
            vec![Effect::PersistAssistant("你好".into()), Effect::EmitLifecycleEnd]
        );
    }

    #[test]
    fn abort_from_streaming() {
        let (s, eff) = step(RunState::Streaming, StepEvent::Abort, "半句");
        assert_eq!(s, RunState::Terminal(RunOutcome::Aborted));
        assert!(matches!(eff[0], Effect::EmitLifecycleError(_)));
    }

    #[test]
    fn error_terminates() {
        let (s, _) = step(RunState::AwaitingModel, StepEvent::ModelError("boom".into()), "");
        assert_eq!(s, RunState::Terminal(RunOutcome::Failed("boom".into())));
    }

    #[test]
    fn tool_call_roundtrip() {
        let (s, eff) = step(
            RunState::Streaming,
            StepEvent::ModelToolCall { call_id: "1".into(), name: "exec".into(), args: "{}".into() },
            "",
        );
        assert_eq!(s, RunState::AwaitingTool);
        assert!(matches!(eff[0], Effect::ExecTool { .. }));

        let (s, eff) = step(s, StepEvent::ToolResult { call_id: "1".into(), output: "ok".into() }, "");
        assert_eq!(s, RunState::AwaitingModel);
        assert_eq!(eff, vec![Effect::CallModel]);
    }

    #[test]
    fn abort_after_terminal_is_noop() {
        let term = RunState::Terminal(RunOutcome::Completed);
        let (s, eff) = step(term.clone(), StepEvent::Abort, "");
        assert_eq!(s, term);
        assert!(eff.is_empty());
    }
}
