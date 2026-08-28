//! Mock provider：脚本化 Delta 流，用于 M3 多轮对话与防卡死的确定性测试。
//!
//! 不联网。可注入每个 Delta 前的延迟，以测试空闲看门狗。

use std::time::Duration;

use futures_util::stream::{self, BoxStream, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::error::LlmResult;
use crate::provider::Provider;
use crate::types::{Delta, FinishReason, ModelRequest, Usage};

/// 一步脚本：延迟 + 要产出的 Delta。
#[derive(Clone)]
pub struct ScriptStep {
    pub delay: Duration,
    pub delta: Delta,
}

/// Mock provider。给定脚本，逐条产出。
pub struct MockProvider {
    script: Vec<ScriptStep>,
}

impl MockProvider {
    /// 从一段回复文本构造：一个 Text delta + Usage + Done(Stop)。
    pub fn echo_text(text: impl Into<String>) -> Self {
        Self {
            script: vec![
                ScriptStep {
                    delay: Duration::ZERO,
                    delta: Delta::Text(text.into()),
                },
                ScriptStep {
                    delay: Duration::ZERO,
                    delta: Delta::Usage(Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    }),
                },
                ScriptStep {
                    delay: Duration::ZERO,
                    delta: Delta::Done(FinishReason::Stop),
                },
            ],
        }
    }

    /// 自定义脚本。
    pub fn scripted(script: Vec<ScriptStep>) -> Self {
        Self { script }
    }

    /// 一个"永远卡住"的脚本：首个 Delta 前延迟极长，用于测试空闲看门狗。
    pub fn stalls_for(delay: Duration) -> Self {
        Self {
            script: vec![ScriptStep {
                delay,
                delta: Delta::Done(FinishReason::Stop),
            }],
        }
    }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }

    async fn stream_chat(
        &self,
        _req: ModelRequest,
        cancel: CancellationToken,
    ) -> LlmResult<BoxStream<'static, LlmResult<Delta>>> {
        let steps = self.script.clone();
        let s = stream::iter(steps).then(move |step| {
            let cancel = cancel.clone();
            async move {
                // 延迟期间响应取消。
                tokio::select! {
                    _ = tokio::time::sleep(step.delay) => Ok(step.delta),
                    _ = cancel.cancelled() => Err(crate::error::ProviderErr::Cancelled),
                }
            }
        });
        Ok(s.boxed())
    }
}
