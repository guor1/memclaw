//! Provider trait（设计 §5）。

use futures_util::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::error::LlmResult;
use crate::types::{Delta, ModelRequest};

/// 一个模型 provider。
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// provider 标识（openai / anthropic / mock）。
    fn id(&self) -> &str;

    /// 发起流式请求。返回增量流。
    ///
    /// 调用方负责在 `cancel` 触发时 drop 该流（看门狗/用户中止）。
    /// 每个 `Delta` 之间的间隔由上层（server）套 timeout 做空闲看门狗。
    async fn stream_chat(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> LlmResult<BoxStream<'static, LlmResult<Delta>>>;
}
