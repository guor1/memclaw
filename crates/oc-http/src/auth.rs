//! Bearer-token middleware for the native and OpenAI API routes.
//!
//! Policy (方案 A):
//! - When no token is configured, the server must be bound to loopback only.
//!   Any address other than 127.0.0.1 or ::1 requires a token at startup.
//! - When a token is configured, every request to `/api/v1/*` and `/v1/*`
//!   must carry `Authorization: Bearer <token>` (or `?token=`, for
//!   `EventSource`). `/health` and the static assets under `/` are exempt so
//!   the browser can reach the page first.
//!
//! The bind-address-vs-token rule is checked by [`check_bind_requires_token`],
//! which the CLI calls before binding the socket, so a misconfiguration fails
//! before any traffic arrives.

use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Middleware: require `Authorization: Bearer <token>` (or `?token=`) when a token is set.
///
/// Routes that do not start with `/api/v1` or `/v1` (i.e. `/health` and
/// static assets) are passed through unconditionally so the browser can load
/// the UI before it knows the token.
///
/// `EventSource` cannot set custom headers, so the ambient-event stream
/// accepts the token as a `token` query parameter as a fallback.
pub async fn require_token(
    axum::extract::State(token): axum::extract::State<Option<String>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = &token else {
        return next.run(req).await;
    };

    let path = req.uri().path();
    if !path.starts_with("/api/v1") && !path.starts_with("/v1") {
        return next.run(req).await;
    }

    // 1. Authorization header (most clients)
    let header_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    // 2. ?token= query param (EventSource fallback)
    let query_token = req.uri().query().and_then(|q| {
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == "token" { Some(v) } else { None }
        })
    });

    let provided = header_token.or(query_token);

    match provided {
        Some(t) if t == expected => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "message": "missing or invalid Authorization: Bearer token",
                    "type": "unauthorized"
                }
            })),
        )
            .into_response(),
    }
}

/// Validate that a non-loopback bind address is paired with a token.
///
/// Call this during startup, before binding the socket. Returns an error
/// message suitable for printing to stderr; the caller should exit.
pub fn check_bind_requires_token(addr: &std::net::SocketAddr, token: &Option<String>) -> Result<(), String> {
    let ip = addr.ip();
    if !ip.is_loopback() && token.is_none() {
        return Err(format!(
            "binding to {addr} exposes the daemon to the network; \
             supply --token (or OC_HTTP_TOKEN) to enable non-loopback binding"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> std::net::SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn loopback_needs_no_token() {
        assert!(check_bind_requires_token(&addr("127.0.0.1:8080"), &None).is_ok());
        assert!(check_bind_requires_token(&addr("[::1]:8080"), &None).is_ok());
    }

    #[test]
    fn non_loopback_requires_token() {
        assert!(check_bind_requires_token(&addr("0.0.0.0:8080"), &None).is_err());
        assert!(check_bind_requires_token(&addr("192.168.1.10:8080"), &None).is_err());
    }

    #[test]
    fn non_loopback_with_token_is_allowed() {
        assert!(check_bind_requires_token(&addr("0.0.0.0:8080"), &Some("s3cret".into())).is_ok());
    }
}
