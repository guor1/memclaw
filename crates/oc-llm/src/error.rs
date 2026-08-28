//! provider 错误。区分可重试与不可重试，供 server 的 failover/retry 决策。

use thiserror::Error;

pub type LlmResult<T> = Result<T, ProviderErr>;

#[derive(Debug, Error)]
pub enum ProviderErr {
    /// 网络/超时/5xx —— 可重试。
    #[error("transient error: {0}")]
    Transient(String),

    /// 认证失败 —— 不可重试（换 key / 换 provider）。
    #[error("auth error: {0}")]
    Auth(String),

    /// 请求非法 / 4xx（非认证）—— 不可重试。
    #[error("invalid request: {0}")]
    Invalid(String),

    /// 限流 —— 可重试（带退避）。
    #[error("rate limited{}", .retry_after_secs.map(|s| format!(" (retry after {s}s)")).unwrap_or_default())]
    RateLimited { retry_after_secs: Option<u64> },

    /// 流被取消（看门狗/用户中止）。
    #[error("stream cancelled")]
    Cancelled,

    /// 解析/协议错误。
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl ProviderErr {
    /// 是否值得重试同一 provider。
    pub fn is_retryable(&self) -> bool {
        matches!(self, ProviderErr::Transient(_) | ProviderErr::RateLimited { .. })
    }
}
