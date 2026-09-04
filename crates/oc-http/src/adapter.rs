//! OpenAI ↔ oc-proto conversion logic.
//!
//! Key design decisions (following OpenClaw):
//! - `previous_response_id` → session reuse via in-memory map
//! - `user` → stable session derivation (hash)
//! - No `user`/`previous_response_id` → per-request ephemeral session

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use dashmap::DashMap;
use oc_proto::SessionId;

use crate::error::{HttpError, HttpResult};
use crate::types::*;

/// Maps `response_id` → `SessionId` for `previous_response_id` continuity.
///
/// In-memory only: a restart loses continuity, which degrades to a new session
/// rather than failing the request. Persisting this would need a store migration;
/// deferred until there is a concrete need.
pub type ResponseSessions = Arc<DashMap<String, SessionId>>;

/// Resolve which session a request should run in.
///
/// Priority: `previous_response_id` (reuse) → `user` (stable derive) → new ephemeral.
pub fn resolve_session(
    req: &CreateResponseReq,
    sessions: &ResponseSessions,
) -> SessionId {
    // 1. previous_response_id: reuse that response's session
    if let Some(prev_id) = &req.previous_response_id {
        if let Some(sid) = sessions.get(prev_id) {
            return sid.clone();
        }
        // Unknown id: fall through rather than erroring — the caller's intent
        // (continue a conversation) is better served by a fresh session than a 404.
    }

    // 2. user: derive a stable session so repeated calls share context
    if let Some(user) = &req.user {
        let mut h = DefaultHasher::new();
        user.hash(&mut h);
        return SessionId::new(format!("http-user-{:x}", h.finish()));
    }

    // 3. Stateless: new session per request
    SessionId::new(format!("http-{}", uuid::Uuid::now_v7()))
}

/// Extracted parts of an OpenAI request that map onto a memclaw turn.
pub struct ExtractedInput {
    /// The "current message" text sent as the user turn.
    pub text: String,
    /// `system`/`developer` messages + `instructions`, appended to the system prompt.
    pub system_additions: Vec<String>,
    /// Decoded `input_file` contents, injected as untrusted external content.
    pub external_files: Vec<ExternalFile>,
    /// Tool results being fed back (`function_call_output` items).
    pub tool_outputs: Vec<ToolOutput>,
}

pub struct ExternalFile {
    pub id: String,
    pub content: String,
    pub filename: Option<String>,
}

pub struct ToolOutput {
    pub call_id: String,
    pub output: String,
}

/// Extract the current message, system additions, files, and tool outputs.
///
/// Mirrors OpenClaw's item semantics: the most recent `user` or
/// `function_call_output` item is the current message; `system`/`developer`
/// go to the system prompt; earlier user/assistant messages are history
/// (memclaw already has them persisted, so they are not re-sent).
pub fn extract_input(req: &CreateResponseReq) -> HttpResult<ExtractedInput> {
    let mut out = ExtractedInput {
        text: String::new(),
        system_additions: Vec::new(),
        external_files: Vec::new(),
        tool_outputs: Vec::new(),
    };

    // `instructions` is appended to the system prompt, never replacing SOUL.md.
    if let Some(inst) = &req.instructions {
        let t = inst.trim();
        if !t.is_empty() {
            out.system_additions.push(t.to_string());
        }
    }

    match &req.input {
        Input::Text(s) => {
            out.text = s.clone();
        }
        Input::Items(items) => {
            // Last user message wins as the current message.
            let mut last_user_text: Option<String> = None;

            for item in items {
                match item {
                    InputItem::Message { role, content } => {
                        let text = flatten_content(content);
                        match role.as_str() {
                            "system" | "developer" => {
                                if !text.trim().is_empty() {
                                    out.system_additions.push(text);
                                }
                            }
                            "user" => {
                                last_user_text = Some(text);
                            }
                            // Earlier assistant turns are already in the
                            // session transcript; ignore to avoid duplication.
                            _ => {}
                        }
                    }
                    InputItem::FunctionCallOutput { call_id, output } => {
                        out.tool_outputs.push(ToolOutput {
                            call_id: call_id.clone(),
                            output: output.clone(),
                        });
                    }
                    InputItem::InputFile { source } => {
                        let (content, filename) = decode_file(source)?;
                        out.external_files.push(ExternalFile {
                            id: format!("file_{}", uuid::Uuid::now_v7()),
                            content,
                            filename,
                        });
                    }
                }
            }

            out.text = last_user_text.unwrap_or_default();
        }
    }

    // A turn needs either a message or a tool result to act on.
    if out.text.trim().is_empty() && out.tool_outputs.is_empty() {
        return Err(HttpError::BadRequest(
            "input must contain a user message or function_call_output".into(),
        ));
    }

    Ok(out)
}

/// Flatten message content to plain text. Unsupported parts are skipped.
fn flatten_content(content: &InputContent) -> String {
    match content {
        InputContent::Text(s) => s.clone(),
        InputContent::Parts(parts) => parts
            .iter()
            .map(|p| match p {
                InputPart::InputText { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Default cap on decoded file text, matching OpenClaw's `files.maxChars`.
const MAX_FILE_CHARS: usize = 60_000;

/// Decode an `input_file` source to text.
///
/// URL sources are rejected: fetching them safely needs DNS/private-IP/redirect
/// guards (see OpenClaw's URL guard), which are out of scope for Phase 1.
/// Sending base64 inline avoids that whole attack surface.
fn decode_file(source: &FileSource) -> HttpResult<(String, Option<String>)> {
    match source {
        FileSource::Base64 { media_type, data, filename } => {
            if !is_text_mime(media_type) {
                return Err(HttpError::BadRequest(format!(
                    "unsupported file media_type: {media_type} (text/plain, text/markdown, \
                     text/html, text/csv, application/json supported)"
                )));
            }
            let bytes = base64_decode(data)
                .ok_or_else(|| HttpError::BadRequest("invalid base64 in input_file".into()))?;
            let mut text = String::from_utf8(bytes)
                .map_err(|_| HttpError::BadRequest("input_file is not valid UTF-8".into()))?;
            if text.chars().count() > MAX_FILE_CHARS {
                text = text.chars().take(MAX_FILE_CHARS).collect::<String>()
                    + "\n…[truncated]";
            }
            Ok((text, filename.clone()))
        }
        FileSource::Url { .. } => Err(HttpError::BadRequest(
            "input_file with url source is not supported; send base64 instead".into(),
        )),
    }
}

fn is_text_mime(mime: &str) -> bool {
    matches!(
        mime,
        "text/plain" | "text/markdown" | "text/html" | "text/csv" | "application/json"
    )
}

/// Minimal standard base64 decoder (avoids adding a dependency).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let cleaned: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    for chunk in cleaned.chunks(4) {
        // A 1-char trailing group cannot encode any byte.
        if chunk.len() < 2 {
            return None;
        }
        let mut acc: u32 = 0;
        for &c in chunk {
            acc = (acc << 6) | val(c)?;
        }
        // Left-align the group so the high bytes land correctly.
        acc <<= 6 * (4 - chunk.len());
        let bytes = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    Some(out)
}

/// Build the text prepended to the user turn to carry per-request context.
///
/// memclaw assembles its system prompt inside the session actor from SOUL.md and
/// config, and the wire protocol (`chat.send`) carries only text. Rather than
/// widen the protocol, per-request instructions and file contents are prefixed
/// to the turn. Files use explicit untrusted-content boundaries so their bytes
/// read as data, not instructions.
pub fn build_turn_prefix(extracted: &ExtractedInput) -> String {
    let mut parts = Vec::new();

    if !extracted.system_additions.is_empty() {
        parts.push(format!(
            "# Instructions for this request\n{}",
            extracted.system_additions.join("\n\n")
        ));
    }

    for f in &extracted.external_files {
        let name = f.filename.as_deref().unwrap_or("unnamed");
        parts.push(format!(
            "<<<EXTERNAL_UNTRUSTED_CONTENT id=\"{}\">>>\n\
             Source: External\nFilename: {}\n\n{}\n\
             <<<END_EXTERNAL_UNTRUSTED_CONTENT id=\"{}\">>>",
            f.id, name, f.content, f.id
        ));
    }

    for t in &extracted.tool_outputs {
        parts.push(format!(
            "# Tool result (call_id: {})\n{}",
            t.call_id, t.output
        ));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", parts.join("\n\n"))
    }
}

/// Validate client tool definitions. Only `function` tools are supported.
pub fn validate_tools(tools: &[ClientTool]) -> HttpResult<()> {
    for t in tools {
        if t.type_ != "function" {
            return Err(HttpError::BadRequest(format!(
                "unsupported tool type '{}': only 'function' tools are supported",
                t.type_
            )));
        }
        if t.name.trim().is_empty() {
            return Err(HttpError::BadRequest("tool name must not be empty".into()));
        }
    }
    Ok(())
}

/// Reject request fields memclaw cannot honor, so callers fail loudly
/// rather than silently getting different behavior.
pub fn reject_unsupported(req: &CreateResponseReq) -> HttpResult<()> {
    if !req.tools.is_empty() {
        return Err(HttpError::BadRequest(
            "dynamic 'tools' are not supported yet; tools are configured server-side".into(),
        ));
    }
    if req.tool_choice.is_some() {
        return Err(HttpError::BadRequest(
            "'tool_choice' is not supported yet".into(),
        ));
    }
    Ok(())
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(input: Input) -> CreateResponseReq {
        CreateResponseReq {
            model: None,
            input,
            instructions: None,
            previous_response_id: None,
            tools: vec![],
            tool_choice: None,
            stream: None,
            store: None,
            max_output_tokens: None,
            temperature: None,
            user: None,
        }
    }

    #[test]
    fn plain_text_input_becomes_turn_text() {
        let r = req(Input::Text("hello".into()));
        let e = extract_input(&r).unwrap();
        assert_eq!(e.text, "hello");
        assert!(e.system_additions.is_empty());
    }

    #[test]
    fn last_user_message_wins_and_system_goes_to_prompt() {
        let r = req(Input::Items(vec![
            InputItem::Message {
                role: "system".into(),
                content: InputContent::Text("be terse".into()),
            },
            InputItem::Message {
                role: "user".into(),
                content: InputContent::Text("first".into()),
            },
            InputItem::Message {
                role: "user".into(),
                content: InputContent::Text("second".into()),
            },
        ]));
        let e = extract_input(&r).unwrap();
        assert_eq!(e.text, "second", "most recent user item is the current message");
        assert_eq!(e.system_additions, vec!["be terse".to_string()]);
    }

    #[test]
    fn instructions_are_appended_not_replacing() {
        let mut r = req(Input::Text("hi".into()));
        r.instructions = Some("You are a Python expert.".into());
        let e = extract_input(&r).unwrap();
        let prefix = build_turn_prefix(&e);
        assert!(prefix.contains("Python expert"));
        assert!(prefix.contains("Instructions for this request"));
    }

    #[test]
    fn empty_input_is_rejected() {
        let r = req(Input::Text("   ".into()));
        assert!(extract_input(&r).is_err());
    }

    #[test]
    fn function_call_output_alone_is_a_valid_turn() {
        let r = req(Input::Items(vec![InputItem::FunctionCallOutput {
            call_id: "call_1".into(),
            output: "{\"temp\":72}".into(),
        }]));
        let e = extract_input(&r).unwrap();
        assert_eq!(e.tool_outputs.len(), 1);
        let prefix = build_turn_prefix(&e);
        assert!(prefix.contains("call_1"));
        assert!(prefix.contains("72"));
    }

    #[test]
    fn base64_file_is_decoded_and_wrapped_as_untrusted() {
        // "Hello World!" in base64
        let r = req(Input::Items(vec![
            InputItem::Message {
                role: "user".into(),
                content: InputContent::Text("summarize".into()),
            },
            InputItem::InputFile {
                source: FileSource::Base64 {
                    media_type: "text/plain".into(),
                    data: "SGVsbG8gV29ybGQh".into(),
                    filename: Some("hello.txt".into()),
                },
            },
        ]));
        let e = extract_input(&r).unwrap();
        assert_eq!(e.external_files.len(), 1);
        assert_eq!(e.external_files[0].content, "Hello World!");
        let prefix = build_turn_prefix(&e);
        assert!(prefix.contains("EXTERNAL_UNTRUSTED_CONTENT"));
        assert!(prefix.contains("hello.txt"));
    }

    #[test]
    fn base64_roundtrip_covers_all_padding_lengths() {
        // Lengths 1..=6 exercise every remainder case in the 4-char groups.
        for (b64, expect) in [
            ("YQ==", "a"),
            ("YWI=", "ab"),
            ("YWJj", "abc"),
            ("YWJjZA==", "abcd"),
            ("YWJjZGU=", "abcde"),
            ("YWJjZGVm", "abcdef"),
        ] {
            let got = base64_decode(b64).expect(b64);
            assert_eq!(String::from_utf8(got).unwrap(), expect, "b64={b64}");
        }
    }

    #[test]
    fn url_file_source_is_rejected() {
        let r = req(Input::Items(vec![
            InputItem::Message {
                role: "user".into(),
                content: InputContent::Text("read it".into()),
            },
            InputItem::InputFile {
                source: FileSource::Url {
                    url: "https://example.com/a.txt".into(),
                    filename: None,
                },
            },
        ]));
        assert!(extract_input(&r).is_err(), "URL fetch has no SSRF guard yet");
    }

    #[test]
    fn same_user_derives_same_session() {
        let sessions: ResponseSessions = Arc::new(DashMap::new());
        let mut a = req(Input::Text("x".into()));
        a.user = Some("alice".into());
        let mut b = req(Input::Text("y".into()));
        b.user = Some("alice".into());
        assert_eq!(resolve_session(&a, &sessions), resolve_session(&b, &sessions));
    }

    #[test]
    fn different_users_get_different_sessions() {
        let sessions: ResponseSessions = Arc::new(DashMap::new());
        let mut a = req(Input::Text("x".into()));
        a.user = Some("alice".into());
        let mut b = req(Input::Text("y".into()));
        b.user = Some("bob".into());
        assert_ne!(resolve_session(&a, &sessions), resolve_session(&b, &sessions));
    }

    #[test]
    fn no_user_yields_fresh_session_each_call() {
        let sessions: ResponseSessions = Arc::new(DashMap::new());
        let a = req(Input::Text("x".into()));
        let b = req(Input::Text("x".into()));
        assert_ne!(
            resolve_session(&a, &sessions),
            resolve_session(&b, &sessions),
            "stateless by default"
        );
    }

    #[test]
    fn previous_response_id_reuses_that_session() {
        let sessions: ResponseSessions = Arc::new(DashMap::new());
        let known = SessionId::new("http-abc");
        sessions.insert("resp_1".to_string(), known.clone());

        let mut r = req(Input::Text("follow up".into()));
        r.previous_response_id = Some("resp_1".into());
        assert_eq!(resolve_session(&r, &sessions), known);
    }

    #[test]
    fn unknown_previous_response_id_degrades_to_new_session() {
        let sessions: ResponseSessions = Arc::new(DashMap::new());
        let mut r = req(Input::Text("follow up".into()));
        r.previous_response_id = Some("resp_missing".into());
        // Does not error; just gets a fresh session.
        let sid = resolve_session(&r, &sessions);
        assert!(sid.as_str().starts_with("http-"));
    }

    #[test]
    fn non_function_tool_is_rejected() {
        let tools = vec![ClientTool {
            type_: "file_search".into(),
            name: "fs".into(),
            description: None,
            parameters: None,
        }];
        assert!(validate_tools(&tools).is_err());
    }
}
