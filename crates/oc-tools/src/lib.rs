//! oc 工具集（设计 §6）。
//!
//! `Tool` trait + 策略管道 + exec 审批门 + 结果净化。M4 首批：exec/file/process。
//! web_fetch/web_search/ask_user/message 紧接着补。

pub mod error;
pub mod exec;
pub mod file;
pub mod process;
pub mod registry;
pub mod sanitize;
pub mod types;

pub use error::{ToolError, ToolResult};
pub use registry::ToolRegistry;
pub use types::*;

use async_trait::async_trait;

/// 一个工具（设计 §6）。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 规格：名称/描述/参数 schema（进 prompt）。
    fn spec(&self) -> ToolSpec;

    /// 策略：是否需审批 / 超时 / 可后台。
    fn policy(&self) -> ToolPolicy;

    /// 执行。
    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput>;
}
