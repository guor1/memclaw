//! 静态资源与 token 注入：/ 服务内嵌 Vue bundle。
//!
//! 编译期经 rust-embed 嵌入 `ui/dist`（含 hashed assets，已提交），release
//! 二进制自包含，无需 Node。服务端把 `--token` 注入 index.html 的
//! `window.__OC_TOKEN__`，浏览器拿到即可带 Bearer 调 `/api/v1/*`。

use axum::{
    extract::State as AxumState,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rust_embed::Embed;

use crate::server::AppState;

#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/ui/dist"]
struct Assets;

/// index.html 里的占位符，serve 时替换为实际 token。
///
/// 值用 `__OC_TOKEN_VALUE__`（而非与变量名相同的 `__OC_TOKEN__`），否则
/// `String::replace` 会连变量名 `window.__OC_TOKEN__` 里的那段一起替换，
/// 把脚本改成 `window.s3cret = "s3cret"` 这种报废代码。
///
/// 页面加载即拿到 token，免去用户手贴。安全的前提是：能到达这个 handler 的
/// 请求，要么已带正确 token（非 loopback），要么本就是 loopback 连接（无鉴权
/// 模式）——注入不会把 token 泄露给未授权的第三方。
const TOKEN_PLACEHOLDER: &str = "__OC_TOKEN_VALUE__";

/// Web UI 路由：`/` 加任意 bundle 资源路径。
///
/// 用 fallback 而非逐文件注册，SPA 自己管客户端路由：未知路径回 index.html，
/// 深链 `/session/abc` 也能正常启动。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .fallback(get(serve_asset))
}

async fn index(AxumState(state): AxumState<AppState>) -> Response {
    serve_path("index.html", state.token.as_deref())
}

async fn serve_asset(AxumState(state): AxumState<AppState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // API 路径绝不回退 index.html：那儿的 404 就该是 404，不能返回 HTML
    // 让 fetch() 调用方解析失败。
    if path.starts_with("api/") || path.starts_with("v1/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if Assets::get(path).is_some() {
        serve_path(path, state.token.as_deref())
    } else {
        // SPA 深链：交还应用外壳，让客户端路由接管。
        serve_path("index.html", state.token.as_deref())
    }
}

fn serve_path(path: &str, token: Option<&str>) -> Response {
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_for(path);
            // 仅 HTML 携带占位符；二进制资源原样返回，省一次无谓的 UTF-8 往返。
            let body: Vec<u8> = if path.ends_with(".html") {
                String::from_utf8_lossy(&file.data)
                    .replace(TOKEN_PLACEHOLDER, token.unwrap_or(""))
                    .into_bytes()
            } else {
                file.data.into_owned()
            };
            (StatusCode::OK, [(header::CONTENT_TYPE, mime)], body).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Web UI bundle not found",
        )
            .into_response(),
    }
}

/// 按扩展名定 content type。不引 mime_guess：Vite 产物只有这几种类型。
fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
