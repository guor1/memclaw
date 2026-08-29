//! web 工具（设计 §6.1）：`web_fetch` 抓取 URL、`web_search` 联网搜索。
//!
//! - **web_fetch**：GET 一个 URL，返回**净化 + 截断**的正文（粗略去 HTML 标签）。
//! - **web_search**：走 DuckDuckGo HTML 端点（无需 key），解析结果标题+链接。
//!
//! 抓来的内容是 **Untrusted**（provenance），由调用方按需归类；本层只做传输 + 净化。
//! 需 `web` feature（默认开）。无 feature 时工具不注册。

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ToolResult};
use crate::sanitize::sanitize;
use crate::types::{ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

/// 抓取正文最大字符数（防拉爆上下文）。
const MAX_BODY_CHARS: usize = 8000;
/// 搜索结果最多条数。
const MAX_RESULTS: usize = 8;

fn client() -> ToolResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("oc-assistant/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ToolError::Failed(format!("构造 HTTP client 失败: {e}")))
}

// ── web_fetch ───────────────────────────────────────────────────

pub struct WebFetchTool;

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".to_string(),
            description: "抓取一个 URL 的网页正文（返回纯文本，已截断）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "http(s) URL" }
                },
                "required": ["url"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: false,
            timeout: std::time::Duration::from_secs(35),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: FetchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        if !(args.url.starts_with("http://") || args.url.starts_with("https://")) {
            return Err(ToolError::BadArgs("url 必须以 http:// 或 https:// 开头".into()));
        }
        cx.update(format!("抓取 {}\n", args.url));

        let resp = tokio::select! {
            _ = cx.cancel.cancelled() => return Err(ToolError::Aborted),
            r = client()?.get(&args.url).send() => r.map_err(|e| ToolError::Failed(format!("请求失败: {e}")))?,
        };
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError::Failed(format!("读取正文失败: {e}")))?;

        let text = html_to_text(&body);
        let truncated: String = text.chars().take(MAX_BODY_CHARS).collect();
        let note = if text.chars().count() > MAX_BODY_CHARS {
            "\n…[正文已截断]"
        } else {
            ""
        };
        Ok(ToolOutput::ok(format!(
            "[{status}] {}\n\n{}{note}",
            args.url,
            sanitize(&truncated)
        )))
    }
}

// ── web_search ──────────────────────────────────────────────────

pub struct WebSearchTool;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".to_string(),
            description: "联网搜索，返回结果标题与链接列表。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: false,
            timeout: std::time::Duration::from_secs(35),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: SearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        cx.update(format!("搜索「{}」\n", args.query));

        // DuckDuckGo HTML 端点（无需 key）。
        let url = "https://html.duckduckgo.com/html/";
        let resp = tokio::select! {
            _ = cx.cancel.cancelled() => return Err(ToolError::Aborted),
            r = client()?.get(url).query(&[("q", args.query.as_str())]).send() => {
                r.map_err(|e| ToolError::Failed(format!("搜索请求失败: {e}")))?
            }
        };
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError::Failed(format!("读取搜索结果失败: {e}")))?;

        let results = parse_ddg_results(&body);
        if results.is_empty() {
            return Ok(ToolOutput::ok(format!("「{}」无搜索结果。", args.query)));
        }
        let mut out = format!("「{}」搜索结果：\n", args.query);
        for (i, (title, link)) in results.iter().take(MAX_RESULTS).enumerate() {
            out.push_str(&format!("{}. {}\n   {}\n", i + 1, title, link));
        }
        Ok(ToolOutput::ok(sanitize(&out)))
    }
}

// ── 解析辅助 ────────────────────────────────────────────────────

/// 极简 HTML → 文本：剥 script/style，去标签，压缩空白。非完美，够喂模型。
fn html_to_text(html: &str) -> String {
    let mut s = strip_block(html, "<script", "</script>");
    s = strip_block(&s, "<style", "</style>");

    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // 解 HTML 实体（常见几个）。
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ");
    // 压缩连续空白/空行。
    let mut result = String::with_capacity(out.len());
    let mut prev_blank = false;
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !prev_blank {
                result.push('\n');
            }
            prev_blank = true;
        } else {
            result.push_str(t);
            result.push('\n');
            prev_blank = false;
        }
    }
    result
}

/// 去掉 `<tag ...>...</close>` 区块（大小写不敏感的起始匹配）。
fn strip_block(s: &str, open: &str, close: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if lower[i..].starts_with(open) {
            if let Some(end) = lower[i..].find(close) {
                i += end + close.len();
                continue;
            } else {
                break; // 无闭合，丢弃剩余
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 从 DuckDuckGo HTML 结果里抽 (标题, 链接)。找 `result__a` 锚点。
fn parse_ddg_results(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // 结果锚：<a ... class="result__a" href="...">标题</a>
    for chunk in html.split("result__a").skip(1) {
        // href
        let Some(href) = extract_attr(chunk, "href=\"") else {
            continue;
        };
        // 标题：紧跟的 `>...</a>`
        let title = chunk
            .split_once('>')
            .and_then(|(_, rest)| rest.split_once("</a>"))
            .map(|(t, _)| strip_tags(t).trim().to_string())
            .unwrap_or_default();
        if !title.is_empty() {
            out.push((title, decode_ddg_href(&href)));
        }
    }
    out
}

fn extract_attr(s: &str, key: &str) -> Option<String> {
    let start = s.find(key)? + key.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// DDG 的 href 常是 `/l/?uddg=<urlencoded>` 跳转，尽量解出真实 URL。
fn decode_ddg_href(href: &str) -> String {
    if let Some(idx) = href.find("uddg=") {
        let enc = &href[idx + 5..];
        let enc = enc.split('&').next().unwrap_or(enc);
        return percent_decode(enc);
    }
    href.to_string()
}

/// 极简 percent-decode（够解 URL）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = hex_val(bytes[i + 1]);
                let l = hex_val(bytes[i + 2]);
                if let (Some(h), Some(l)) = (h, l) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_strips_tags_and_scripts() {
        let html = "<html><head><style>x{}</style></head><body><p>你好<script>bad()</script>世界</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("你好"));
        assert!(text.contains("世界"));
        assert!(!text.contains("bad()"), "script 应被剥除");
        assert!(!text.contains("x{}"), "style 应被剥除");
    }

    #[test]
    fn entity_decoding() {
        assert_eq!(html_to_text("<p>a &amp; b &lt;c&gt;</p>").trim(), "a & b <c>");
    }

    #[test]
    fn percent_decode_url() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com%2Fa+b"),
            "https://example.com/a b"
        );
    }

    #[test]
    fn parse_ddg_extracts_title_and_link() {
        let html = r#"<a class="result__a" href="/l/?uddg=https%3A%2F%2Frust-lang.org%2F">Rust 官网</a>"#;
        let r = parse_ddg_results(html);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "Rust 官网");
        assert_eq!(r[0].1, "https://rust-lang.org/");
    }
}
