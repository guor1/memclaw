//! server 错误类型。

use thiserror::Error;

pub type ServerResult<T> = Result<T, ServerError>;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("store error: {0}")]
    Store(#[from] oc_store::StoreError),

    #[error("codec error: {0}")]
    Codec(String),

    #[error("transport unsupported on this platform: {0}")]
    UnsupportedTransport(String),
}
