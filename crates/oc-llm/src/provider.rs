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

    /// 实际请求的 API 基地址。`None` = 无网络端点（mock）。
    ///
    /// 为什么不从 config 的 `base_url` 取：那里 `None` 表示「用官方默认」，取用时
    /// 得在调用侧重复一遍默认值，容易与 provider 内部的默认漂移。这里返回的是
    /// provider 真正拼进 URL 的那个串。
    ///
    /// 尤其重要的是 OpenAI 兼容端点：DeepSeek / 豆包 / Kimi 的 [`Self::id`] 都是
    /// `"openai"`，只有基地址能区分请求实际打到哪家。
    fn endpoint(&self) -> Option<&str> {
        None
    }

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
