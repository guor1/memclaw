//! oc 模型传输（设计 §5）。
//!
//! Provider trait + 统一 `Delta` 流。**只管传输，不管循环编排**（编排在 server
//! 用 oc-core 的状态机）。两 provider 差异归一到统一 `Delta`。

pub mod error;
pub mod provider;
pub mod sse;
pub mod types;

#[cfg(feature = "openai")]
pub mod openai;
#[cfg(feature = "anthropic")]
pub mod anthropic;

pub mod mock;

pub use error::{ProviderErr, LlmResult};
pub use provider::Provider;
pub use types::*;
