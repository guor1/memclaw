//! 上下文摘要 prompt 组装（设计 §4 compaction 第 3 层）。纯函数，可单测。
//!
//! 参考 OpenClaw `/compact`：把旧对话总结成**结构化 checkpoint**，供后续轮次继续
//! 工作时读取。要求保留确切的文件路径、函数名、错误信息。调模型的 IO 在 server。

/// 摘要系统提示：只输出结构化摘要，不续对话。
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "你是上下文摘要助手。你的任务是阅读用户与 AI 助手的对话，\
产出一份严格遵循指定格式的结构化摘要。\
不要续写对话，不要回答对话中的任何问题，只输出结构化摘要。";

/// 一条参与摘要的消息（角色 + 内容）。
pub struct SummaryMsg<'a> {
    pub role: &'a str,
    pub content: &'a str,
}

/// 结构化摘要模板尾注（附在对话之后，指示输出格式）。
const SUMMARY_FORMAT: &str = r#"以上是需要总结的对话。请生成一份结构化的上下文检查点摘要，供另一个 LLM 用于继续工作。

严格使用以下格式：

## 目标
[用户想完成什么？若会话涉及多个任务可列多条。]

## 约束与偏好
- [用户提到的任何约束、偏好或要求]
- [若无则写 "(无)"]

## 进度
### 已完成
- [x] [已完成的任务/改动]

### 进行中
- [ ] [当前工作]

### 受阻
- [阻碍进展的问题，若有]

## 关键决策
- **[决策]**：[简要理由]

## 下一步
1. [按顺序列出接下来该做什么]

## 关键上下文
- [继续工作所需的数据、示例或引用]
- [若不适用则写 "(无)"]

每节保持简洁。保留确切的文件路径、函数名和错误信息。"#;

/// 组装一次全新摘要的用户提示：对话正文 + 格式模板。
pub fn build_summary_prompt(messages: &[SummaryMsg]) -> String {
    let mut s = String::new();
    for m in messages {
        s.push_str(&format!("[{}] {}\n", m.role, m.content));
    }
    s.push('\n');
    s.push_str(SUMMARY_FORMAT);
    s
}

/// 组装增量更新摘要的用户提示：把新消息并入既有摘要（避免从头重总结）。
pub fn build_summary_update_prompt(prev_summary: &str, new_messages: &[SummaryMsg]) -> String {
    let mut s = String::new();
    s.push_str("<previous-summary>\n");
    s.push_str(prev_summary.trim());
    s.push_str("\n</previous-summary>\n\n");
    s.push_str("以下是需要并入既有摘要的新对话消息：\n\n");
    for m in new_messages {
        s.push_str(&format!("[{}] {}\n", m.role, m.content));
    }
    s.push('\n');
    s.push_str("在保持同一结构化格式的前提下，将新消息整合进上述摘要，输出更新后的完整摘要。");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_prompt_has_structure_and_messages() {
        let msgs = vec![
            SummaryMsg { role: "user", content: "帮我改 main.rs 的解析函数" },
            SummaryMsg { role: "assistant", content: "已定位 parse_args()" },
        ];
        let p = build_summary_prompt(&msgs);
        assert!(p.contains("main.rs"), "应含对话内容");
        assert!(p.contains("## 目标"), "应含结构化字段");
        assert!(p.contains("## 下一步"));
        assert!(p.contains("文件路径"), "应要求保留文件路径等");
    }

    #[test]
    fn update_prompt_wraps_previous_summary() {
        let msgs = vec![SummaryMsg { role: "user", content: "再改 config.rs" }];
        let p = build_summary_update_prompt("## 目标\n改代码", &msgs);
        assert!(p.contains("<previous-summary>"));
        assert!(p.contains("改代码"), "应含既有摘要");
        assert!(p.contains("config.rs"), "应含新消息");
    }
}
