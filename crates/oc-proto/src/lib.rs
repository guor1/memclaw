//! oc 交互协议：CLI/TUI ↔ daemon 的线上契约（DTO）。
//!
//! 见设计文档 §2。本 crate **只有类型 + serde + schemars**，无逻辑、无 IO。
//! 帧模型三类：`Req` / `Res` / `Event`，一律判别联合（tagged enum），
//! 让"不可能的状态无法表示"。

pub mod ids;
pub mod frame;
pub mod method;
pub mod event;
pub mod schema;

pub use event::*;
pub use frame::*;
pub use ids::*;
pub use method::*;

/// 协议版本。单用户下不做 N-1 兼容协商，只做硬校验（不匹配拒连）。
///
/// v2：Event 加 `session` 字段 + 多会话方法（sessions.list / session_reset 带 session）。
pub const PROTO_VERSION: u16 = 2;
