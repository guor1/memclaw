//! 模型输出被 max_tokens 截断（`finish_reason=length`）时的收敛行为。
//!
//! 真机症状（2026-09-09 日志）：thinking 模型把输出预算全烧在 reasoning 上，
//! 106 秒只吐 46 个可见字符就 `reason=Length`，而 run 把它当正常结束——
//! 用户看到一句半截话，任务（跑脚本生成 PPT）一个工具都没调就"完成"了。
//!
//! 正确行为：Length 等同"回合未完成"，续写；续不出来就报错，不静默成功。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::{Delta, FinishReason};
use oc_proto::{Event, LifecyclePhase};
use oc_server::session;
use oc_server::testing::test_cfg;
use tokio::sync::broadcast;

/// 一轮：吐一段文本后因 max_tokens 截断。
fn truncated_step(t: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text(t.into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Length) },
    ]
}

/// 一轮：只有 reasoning（thinking 模型烧光预算的真实形状），无可见文本。
fn reasoning_only_truncated_step(r: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Reasoning(r.into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Length) },
    ]
}

fn text_step(t: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text(t.into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ]
}

async fn collect_until_terminal(
    rx: &mut broadcast::Receiver<Event>,
    timeout: Duration,
) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        let terminal = matches!(
            ev,
            Event::Lifecycle { phase: LifecyclePhase::End, .. }
                | Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
        );
        out.push(ev);
        if terminal {
            break;
        }
    }
    out
}

async fn run_scripts(scripts: Vec<Vec<ScriptStep>>) -> Vec<Event> {
    run_scripts_with_store(scripts).await.0
}

/// 同上，另外把 store 交回来供落库侧断言。
async fn run_scripts_with_store(
    scripts: Vec<Vec<ScriptStep>>,
) -> (Vec<Event>, oc_store::Store) {
    let (tx, mut rx) = broadcast::channel(512);
    let provider = Arc::new(SequencedMock::new(scripts));
    let sid = oc_proto::SessionId::main();
    let store = oc_store::Store::open_memory().unwrap();
    let handle = session::spawn(
        sid.clone(),
        test_cfg(),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    let _run = handle
        .submit("生成一份大话西游人物介绍PPT".into(), handle.broadcast_sink())
        .await
        .expect("run");
    let evs = collect_until_terminal(&mut rx, Duration::from_secs(5)).await;
    (evs, store)
}

fn assistant_text(evs: &[Event]) -> String {
    evs.iter()
        .filter_map(|e| match e {
            Event::Assistant { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn length_finish_continues_instead_of_ending() {
    // 第一轮被截断在半句话，第二轮补完。
    let evs = run_scripts(vec![
        truncated_step("环境没问题，python-pptx 已就绪。我来设计并生成这份 PPT。OLD"),
        text_step("已生成 slides.pptx，共 6 页。"),
    ])
    .await;

    let text = assistant_text(&evs);
    assert!(
        text.contains("已生成 slides.pptx"),
        "截断后应续写出完整回答，实际只有：{text}"
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::Lifecycle { phase: LifecyclePhase::End, .. })),
        "续写成功后应正常结束"
    );
}

#[tokio::test]
async fn length_finish_exhausting_retries_reports_error() {
    // 每轮都被截断：续写用尽后必须报错，不能静默当成功。
    let evs = run_scripts(vec![
        truncated_step("半句"),
        truncated_step("又半句"),
        truncated_step("还是半句"),
        truncated_step("永远半句"),
    ])
    .await;

    assert!(
        evs.iter().any(|e| matches!(
            e,
            Event::Lifecycle {
                phase: LifecyclePhase::Error { kind: oc_proto::RunErrorKind::Truncated, .. },
                ..
            }
        )),
        "续写次数用尽应发 Truncated 错误，实际事件：{evs:?}"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, Event::Lifecycle { phase: LifecyclePhase::End, .. })),
        "截断未收敛不得以正常 End 结束"
    );
}

#[tokio::test]
async fn reasoning_only_truncation_still_continues() {
    // thinking 模型烧光预算：一个可见字符都没有。这正是真机上那一轮的形状。
    let evs = run_scripts(vec![
        reasoning_only_truncated_step("我需要先确认 python-pptx 版本，然后……"),
        text_step("已生成 slides.pptx。"),
    ])
    .await;

    let text = assistant_text(&evs);
    assert!(
        text.contains("已生成 slides.pptx"),
        "纯 reasoning 截断轮也要续写，实际：{text}"
    );
}

/// 工具调用参数写到一半被截断，续写指令必须换成「拆小重做」。
///
/// 这是真机上 PPT 任务的真实形状：模型把整个 Python 脚本当作 exec 的
/// arguments 流式吐出，75 秒后撞上限，JSON 断在半路。残缺参数是非法 JSON、
/// 进不了历史，模型看不到自己刚写了什么，「接着写」对它没有意义——它只会
/// 原样重写，在同一处再被砍。真机上连续三轮就是这么废掉的。
#[tokio::test]
async fn tool_arg_truncation_tells_model_to_split_work() {
    let (tx, mut rx) = broadcast::channel(512);
    // 第一轮：吐一段超长 exec 参数后被截断；第二轮：改用小步骤，正常收尾。
    let scripts = vec![
        vec![
            ScriptStep {
                delay: Duration::ZERO,
                delta: Delta::Text("我来写脚本生成 PPT。".into()),
            },
            ScriptStep {
                delay: Duration::ZERO,
                delta: Delta::ToolCall(oc_llm::types::ToolCallDelta {
                    call_id: "call-huge".into(),
                    name: Some("exec".into()),
                    // 半截 JSON：真机上就是这样断的。
                    args_chunk: "{\"command\": \"python3 -c \\\"from pptx import".into(),
                }),
            },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Length) },
        ],
        text_step("我改成分步写文件。"),
    ];
    let provider = Arc::new(SequencedMock::new(scripts));
    let captures = provider.captures();
    let sid = oc_proto::SessionId::main();
    let handle = session::spawn(
        sid.clone(),
        test_cfg(),
        provider,
        tx,
        oc_store::Store::open_memory().unwrap(),
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    handle
        .submit("生成一份大话西游人物介绍PPT".into(), handle.broadcast_sink())
        .await
        .expect("run");
    collect_until_terminal(&mut rx, Duration::from_secs(5)).await;

    // 第二次请求里应带「拆小重做」的指令，而不是「接着写完」。
    let reqs = captures.lock().unwrap();
    let second = reqs.get(1).expect("应有续写请求");
    let nudge = second
        .messages
        .last()
        .expect("续写请求末尾应是指令");
    assert!(
        nudge.content.contains("拆小") || nudge.content.contains("分多次"),
        "工具参数截断应指导拆小重做，实际：{}",
        nudge.content
    );
    assert!(
        !nudge.content.contains("紧接着截断处继续写完"),
        "不该让模型「接着写」——残缺参数它根本看不到：{}",
        nudge.content
    );
}

///
/// 分成几条落库的话，重放时就是几段各自读不通的碎片，模型照着学，
/// 下一轮接着说半句。
#[tokio::test]
async fn successful_continuation_persists_one_merged_reply() {
    let (_evs, store) = run_scripts_with_store(vec![
        truncated_step("环境就绪，我来写脚本。"),
        text_step("已生成 slides.pptx。"),
    ])
    .await;

    let hist = store.writer().load_transcript("main".into(), 100).await.unwrap();
    let replies: Vec<&str> = hist
        .iter()
        .filter(|e| e.role == oc_store::Role::Assistant)
        .map(|e| e.content.as_str())
        .collect();

    assert_eq!(replies.len(), 1, "应只落一条合并后的回复，实际：{replies:?}");
    assert!(
        replies[0].contains("环境就绪") && replies[0].contains("slides.pptx"),
        "合并后的回复应含两段，实际：{}",
        replies[0]
    );
}

/// 续写用尽而失败的 run，一个残片都不该留在库里。
///
/// 真机上留下了 ast:38 / ast:1 / ast:11 三条碎片，后面每一轮都拿它们当上下文，
/// 于是排队轮回「PPT 还没做完，我这就继续生成」——一次失败变成了持续污染。
#[tokio::test]
async fn exhausted_continuation_persists_no_fragments() {
    let (_evs, store) = run_scripts_with_store(vec![
        truncated_step("半句"),
        truncated_step("又半句"),
        truncated_step("还是半句"),
        truncated_step("永远半句"),
    ])
    .await;

    let hist = store.writer().load_transcript("main".into(), 100).await.unwrap();
    let replies: Vec<&str> = hist
        .iter()
        .filter(|e| e.role == oc_store::Role::Assistant)
        .map(|e| e.content.as_str())
        .collect();

    assert!(
        replies.is_empty(),
        "失败的截断 run 不得在库里留下残片，实际：{replies:?}"
    );
}
