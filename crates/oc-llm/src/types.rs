//! provider 无关的请求/响应类型（设计 §5）。

use serde::{Deserialize, Serialize};

/// 发给模型的一次请求。
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

/// 对话消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MsgRole,
    pub content: String,
    /// 若此消息是工具结果，关联的调用 id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MsgRole {
    System,
    User,
    Assistant,
    Tool,
}

/// 工具规格（进 prompt，供模型决定调用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema 描述参数。
    pub parameters: serde_json::Value,
}

/// 流式增量（两 provider 归一到此）。
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    /// 文本片段。
    Text(String),
    /// 工具调用增量（分片拼装）。
    ToolCall(ToolCallDelta),
    /// token 计量。
    Usage(Usage),
    /// 结束。
    Done(FinishReason),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallDelta {
    pub call_id: String,
    pub name: Option<String>,
    /// 参数 JSON 的增量片段（累积后解析）。
    pub args_chunk: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// 模型自然结束（无更多工具调用）。
    Stop,
    /// 模型请求调用工具。
    ToolUse,
    /// 达到 max_tokens。
    Length,
}
