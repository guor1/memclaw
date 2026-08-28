//! oc 本地终端 UI（设计 §8）。协议的第一个 client。
//!
//! M2：连上 daemon、发消息、显示流式事件。纯展示，无业务逻辑。

pub mod app;
pub mod client;

pub use client::ClientTransport;
