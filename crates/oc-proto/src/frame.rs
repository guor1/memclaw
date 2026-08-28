//! 顶层帧模型：`Req` / `Res` / `Event`（设计 §2.1）。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::ids::{IdemKey, ReqId};
use crate::method::{Method, MethodOk};

/// 顶层帧。`tag = "kind"` 让不可能的状态无法表示。
///
/// 线上编码：NDJSON（每帧一行 JSON），见设计 §2.5。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    /// client → server，期待一个 `Res`。
    Req(Req),
    /// server → client，对某个 `req` 的应答。
    Res(Res),
    /// server → client，主动推送，无对应 `req`。
    Event(Event),
}

/// 请求帧。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Req {
    /// client 生成，用于配对 `Res`。
    pub id: ReqId,
    /// 方法（判别联合，见 [`Method`]）。
    pub method: Method,
    /// 仅 side-effecting 方法（如 `chat.send`）要求，用于幂等去重。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdemKey>,
}

/// 应答帧。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Res {
    /// 对应的 `req.id`。
    pub id: ReqId,
    /// 结果或协议错误。
    pub result: ResResult,
}

/// `Res` 的载荷。用显式 tagged enum 而非 `Result`，以便 schema 清晰。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResResult {
    Ok(MethodOk),
    Err(ProtoError),
}

/// 协议层错误。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProtoError {
    pub kind: ErrorKind,
    pub message: String,
}

/// 错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// 协议版本不匹配（拒连）。
    ProtoVersionMismatch,
    /// 请求格式非法。
    BadRequest,
    /// 方法不支持（未启用的 feature 等）。
    Unsupported,
    /// 内部错误。
    Internal,
    /// 被中止。
    Aborted,
    /// 超时。
    Timeout,
}
