//! HTTP error types and status code mapping.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::Json;
use serde_json::json;

pub type HttpResult<T> = Result<T, HttpError>;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("connection error: {0}")]
    Connection(String),
}

impl HttpError {
    pub fn from_proto(e: oc_proto::ProtoError) -> Self {
        match e.kind {
            oc_proto::ErrorKind::BadRequest => Self::BadRequest(e.message),
            oc_proto::ErrorKind::Unsupported => Self::BadRequest(format!("unsupported: {}", e.message)),
            oc_proto::ErrorKind::Aborted => Self::Internal(format!("aborted: {}", e.message)),
            oc_proto::ErrorKind::Timeout => Self::Internal(format!("timeout: {}", e.message)),
            _ => Self::Internal(e.message),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> AxumResponse {
        let (status, code) = match &self {
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
            Self::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Self::Protocol(_) => (StatusCode::BAD_GATEWAY, "protocol_error"),
            Self::Internal(_) | Self::Connection(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };

        let body = json!({
            "error": {
                "message": self.to_string(),
                "type": code,
            }
        });

        (status, Json(body)).into_response()
    }
}
