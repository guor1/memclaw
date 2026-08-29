//! oc 领域层：纯策略集合（设计 §4）。
//!
//! **不 spawn task、不开连接、不读时钟、不 rand**。所有外部量作为输入参数传入，
//! core 输出决策/计划，由 oc-server 执行。这让 core 100% 可确定性单测。
//!
//! M1 仅落地 `config`；其余模块（agent/queue/prompt/memory/proactive/...）随里程碑加入。

pub mod agent;
pub mod compaction;
pub mod config;
pub mod memory;
pub mod model;
pub mod prompt;
pub mod queue;
pub mod tool;

pub use config::{Config, ReloadKind, SecretRef};
