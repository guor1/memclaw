//! 工具错误。

use thiserror::Error;

pub type ToolResult<T> = Result<T, ToolError>;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("参数错误: {0}")]
    BadArgs(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("工具超时")]
    Timeout,

    #[error("被中止")]
    Aborted,

    #[error("被审批拒绝")]
    Denied,

    #[error("路径不允许: {0}")]
    PathNotAllowed(String),

    #[error("执行失败: {0}")]
    Failed(String),
}
