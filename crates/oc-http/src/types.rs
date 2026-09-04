//! OpenAI Responses API types (CreateResponse / Response / OutputItem).

use serde::{Deserialize, Serialize};

// ─── Request ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateResponseReq {
    #[serde(default)]
    pub model: Option<String>,
    pub input: Input,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub tools: Vec<ClientTool>,
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub user: Option<String>,
    // Accept but ignore: metadata, reasoning, truncation
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Input {
    Text(String),
    Items(Vec<InputItem>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Message {
        role: String,
        content: InputContent,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
    #[serde(rename = "input_file")]
    InputFile {
        source: FileSource,
    },
    // Accept but ignore: reasoning, item_reference
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum InputContent {
    Text(String),
    Parts(Vec<InputPart>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    // Phase 2: input_image
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileSource {
    Base64 {
        media_type: String,
        data: String,
        #[serde(default)]
        filename: Option<String>,
    },
    Url {
        url: String,
        #[serde(default)]
        filename: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
pub struct ClientTool {
    #[serde(rename = "type")]
    pub type_: String, // Must be "function"
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    String(String), // "auto" | "none" | "required"
    Specific {
        #[serde(rename = "type")]
        type_: String,
        name: String,
    },
}

// ─── Response ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Response {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
    pub output: Vec<OutputItem>,
    pub usage: Usage,
    pub model: String,
    // Simplified: omit instructions, reasoning, tools, etc.
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Serialize)]
pub struct ResponseError {
    pub message: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Message {
        id: String,
        status: String,
        role: String,
        content: Vec<ContentPart>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        annotations: Vec<serde_json::Value>,
    },
}

#[derive(Debug, Serialize, Default)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
}

// ─── SSE Events ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SseEvent {
    #[serde(rename = "response.created")]
    ResponseCreated {
        response: Response,
        sequence_number: u64,
    },
    #[serde(rename = "response.in_progress")]
    ResponseInProgress {
        response_id: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        item_id: String,
        output_index: usize,
        item: OutputItem,
        sequence_number: u64,
    },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        item_id: String,
        output_index: usize,
        content_index: usize,
        part: ContentPart,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        item_id: String,
        output_index: usize,
        content_index: usize,
        delta: String,
        sequence_number: u64,
        logprobs: Vec<serde_json::Value>,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        item_id: String,
        output_index: usize,
        content_index: usize,
        text: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        item_id: String,
        output_index: usize,
        item: OutputItem,
        sequence_number: u64,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted {
        response: Response,
        sequence_number: u64,
    },
    #[serde(rename = "response.failed")]
    ResponseFailed {
        response_id: String,
        error: ResponseError,
        sequence_number: u64,
    },
}
