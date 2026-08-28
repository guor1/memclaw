//! 强类型 ID 与标识符。用 newtype 包裹字符串/UUID，避免混用。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }
    };
}

string_id!(
    /// client 生成的请求 id，用于配对 `Res`。
    ReqId
);
string_id!(
    /// 幂等键（client 生成，建议 UUIDv7）。
    IdemKey
);
string_id!(
    /// 一次 agent run 的 id。
    RunId
);
string_id!(
    /// 会话 id（"main" 或子会话 uuid）。
    SessionId
);
string_id!(ToolCallId);
string_id!(CronId);
string_id!(TaskId);
string_id!(MemoryId);
string_id!(
    /// 一次审批请求的 id，用于配对审批回执。
    ApprovalId
);

impl SessionId {
    /// 主会话固定 id。
    pub fn main() -> Self {
        Self("main".to_string())
    }
}
