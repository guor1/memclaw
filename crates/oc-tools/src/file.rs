//! file 工具（设计 §6.1）：读/写/列文件。
//!
//! 路径策略：限制在允许的根目录内（防越权读写）。写操作由 server 记审计。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ToolResult};
use crate::path_guard::{expand_home, resolve_in_roots};
use crate::sanitize::sanitize;
use crate::types::{ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

/// head/tail 默认行数。
const DEFAULT_LINES: usize = 20;
/// grep/glob 结果上限（防拉爆上下文）。
const MAX_MATCHES: usize = 200;

/// file 工具。允许的根目录列表；空 = 允许任意（信任环境）。
pub struct FileTool {
    allowed_roots: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FileArgs {
    Read { path: String },
    Write { path: String, content: String },
    /// 精确替换一段文本。`old_string` 默认必须唯一匹配。
    ///
    /// 存在的意义是省流量：模型改一行也走 `write` 的话得重发整个文件，而参数
    /// 长度直接决定流式耗时（真机量到约 48 字符/秒，20KB 文件覆写要 7 分钟）。
    Edit {
        path: String,
        old_string: String,
        new_string: String,
        /// 允许替换全部出现处。默认 false（歧义时报错，不猜）。
        #[serde(default)]
        replace_all: bool,
    },
    /// 追加到文件末尾，文件不存在则创建。
    ///
    /// 长内容分多次追加可以避开单次工具参数撞 max_tokens 被截断
    /// （见 run.rs 的 TOOL_TRUNCATION_INSTRUCTION）。
    Append { path: String, content: String },
    List { path: String },
    /// 文件元信息：大小/是否目录/修改时间。
    Stat { path: String },
    /// 文件头 N 行（默认 20）。
    Head { path: String, #[serde(default)] lines: Option<usize> },
    /// 文件尾 N 行（默认 20）。
    Tail { path: String, #[serde(default)] lines: Option<usize> },
    /// 在文件/目录内按正则搜内容，返回 路径:行号:内容。
    Grep { pattern: String, path: String },
    /// 按 glob 模式找文件（如 "**/*.rs"），相对 cwd。
    Glob { pattern: String },
}

impl FileTool {
    pub fn new(allowed_roots: Vec<PathBuf>) -> Self {
        Self { allowed_roots }
    }

    /// 解析路径（相对 cwd）并校验在允许范围内。委托给共享的 path_guard。
    ///
    /// 先展开开头的 `~`（`~/.oc/skills/...` → 用户主目录），否则会被当成
    /// cwd 下的字面 `~` 目录而解析失败。
    fn check(&self, path: &Path, cwd: &Path) -> ToolResult<PathBuf> {
        let expanded = expand_home(&path.to_string_lossy());
        resolve_in_roots(Path::new(&expanded), cwd, &self.allowed_roots)
    }
}

#[async_trait]
impl Tool for FileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "file".to_string(),
            description: "文件操作（优先于 exec）。op=read|write|edit|append|list|stat|head|tail|grep|glob。\
                修改已有文件用 edit（只发要改的片段）而不是 write 重发整个文件。\
                路径可相对当前工作目录。\
                技能正文在 ~/.oc/skills/<name>/SKILL.md（用 op=read 按需读取）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["read", "write", "edit", "append", "list", "stat", "head", "tail", "grep", "glob"] },
                    "path": { "type": "string", "description": "文件/目录路径（read/write/edit/append/list/stat/head/tail/grep 用）" },
                    "content": { "type": "string", "description": "write/append 时的内容" },
                    "old_string": { "type": "string", "description": "edit：要被替换的原文，必须与文件内容逐字符一致（含缩进）。默认须唯一匹配，出现多次时请多带上下文" },
                    "new_string": { "type": "string", "description": "edit：替换成的新内容" },
                    "replace_all": { "type": "boolean", "description": "edit：替换全部出现处（默认 false）" },
                    "lines": { "type": "integer", "description": "head/tail 的行数（默认 20）" },
                    "pattern": { "type": "string", "description": "grep 的正则 / glob 的模式（如 **/*.rs）" }
                },
                "required": ["op"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: false,
            timeout: std::time::Duration::from_secs(30),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: FileArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::BadArgs(e.to_string()))?;

        match args {
            FileArgs::Read { path } => {
                let p = self.check(Path::new(&path), &cx.cwd)?;
                cx.update(format!("读取 {}\n", p.display()));
                let content = tokio::fs::read_to_string(&p).await?;
                Ok(ToolOutput::ok(sanitize(&content)))
            }
            FileArgs::Write { path, content } => {
                let p = self.check(Path::new(&path), &cx.cwd)?;
                if let Some(parent) = p.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&p, content.as_bytes()).await?;
                cx.update(format!("写入 {} ({} 字节)\n", p.display(), content.len()));
                Ok(ToolOutput::ok(format!("已写入 {}", p.display())))
            }
            FileArgs::Edit { path, old_string, new_string, replace_all } => {
                // 这两种参数是模型出错，不是「改写没匹配上」，归 BadArgs：
                // 空 old_string 会匹配任意位置；old == new 是无操作。
                if old_string.is_empty() {
                    return Err(ToolError::BadArgs(
                        "old_string 不能为空（空串匹配任意位置）".into(),
                    ));
                }
                if old_string == new_string {
                    return Err(ToolError::BadArgs(
                        "old_string 与 new_string 相同，这次调用不会改变任何内容".into(),
                    ));
                }
                let p = self.check(Path::new(&path), &cx.cwd)?;
                let content = tokio::fs::read_to_string(&p).await?;

                // 匹配失败一律返回 ToolOutput::err 而非 ToolError：前者的文案会被
                // 原样回喂给模型（后者会加「工具错误: 」前缀）。文案要能让模型自纠，
                // 否则它只会原样重试，往返次数反而比 write 更多。
                let Some((needle, replacement)) =
                    resolve_needle(&content, &old_string, &new_string)
                else {
                    return Ok(ToolOutput::err(no_match_hint(&content, &old_string)));
                };

                let count = content.matches(needle.as_str()).count();
                if count > 1 && !replace_all {
                    return Ok(ToolOutput::err(format!(
                        "old_string 在文件中出现 {count} 次，无法确定改哪一处。\
                         请补上更多上下文（前后各多带几行）使其唯一，\
                         或确实要全改时传 replace_all=true。文件未改动。"
                    )));
                }

                let updated = if replace_all {
                    content.replace(needle.as_str(), &replacement)
                } else {
                    content.replacen(needle.as_str(), &replacement, 1)
                };
                let before = content.len();
                let after = updated.len();
                tokio::fs::write(&p, updated.as_bytes()).await?;
                cx.update(format!("编辑 {} ({count} 处)\n", p.display()));
                Ok(ToolOutput::ok(format!(
                    "已编辑 {}：替换 {count} 处，{before} → {after} 字节",
                    p.display()
                )))
            }
            FileArgs::Append { path, content } => {
                let p = self.check(Path::new(&path), &cx.cwd)?;
                if let Some(parent) = p.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                use tokio::io::AsyncWriteExt;
                let mut f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&p)
                    .await?;
                f.write_all(content.as_bytes()).await?;
                f.flush().await?;
                let total = tokio::fs::metadata(&p).await.map(|m| m.len()).unwrap_or(0);
                cx.update(format!("追加 {} ({} 字节)\n", p.display(), content.len()));
                Ok(ToolOutput::ok(format!(
                    "已追加 {} 字节到 {}，现共 {total} 字节",
                    content.len(),
                    p.display()
                )))
            }
            FileArgs::List { path } => {
                let p = self.check(Path::new(&path), &cx.cwd)?;
                let mut entries = tokio::fs::read_dir(&p).await?;
                let mut out = String::new();
                while let Some(e) = entries.next_entry().await? {
                    let ft = e.file_type().await?;
                    let mark = if ft.is_dir() { "/" } else { "" };
                    out.push_str(&format!("{}{}\n", e.file_name().to_string_lossy(), mark));
                }
                Ok(ToolOutput::ok(sanitize(&out)))
            }
            FileArgs::Stat { path } => {
                let p = self.check(Path::new(&path), &cx.cwd)?;
                let md = tokio::fs::metadata(&p).await?;
                let kind = if md.is_dir() { "目录" } else { "文件" };
                let modified = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(ToolOutput::ok(format!(
                    "{}\n类型: {kind}\n大小: {} 字节\n修改时间(unix秒): {modified}",
                    p.display(),
                    md.len()
                )))
            }
            FileArgs::Head { path, lines } => {
                let p = self.check(Path::new(&path), &cx.cwd)?;
                let content = tokio::fs::read_to_string(&p).await?;
                let n = lines.unwrap_or(DEFAULT_LINES);
                let out: String = content.lines().take(n).collect::<Vec<_>>().join("\n");
                Ok(ToolOutput::ok(sanitize(&out)))
            }
            FileArgs::Tail { path, lines } => {
                let p = self.check(Path::new(&path), &cx.cwd)?;
                let content = tokio::fs::read_to_string(&p).await?;
                let n = lines.unwrap_or(DEFAULT_LINES);
                let all: Vec<&str> = content.lines().collect();
                let start = all.len().saturating_sub(n);
                let out = all[start..].join("\n");
                Ok(ToolOutput::ok(sanitize(&out)))
            }
            FileArgs::Grep { pattern, path } => {
                let p = self.check(Path::new(&path), &cx.cwd)?;
                // grep 用同步 walkdir/std::fs 遍历目录树：大目录会长时间占用
                // 当前 tokio worker，饿死同线程上的其它会话/心跳。放到阻塞线程池，
                // 让 async executor 不被同步循环霸占（工具级超时也才对它生效）。
                tokio::task::spawn_blocking(move || grep(&pattern, &p))
                    .await
                    .map_err(|e| ToolError::Failed(format!("grep 任务失败: {e}")))?
            }
            FileArgs::Glob { pattern } => {
                // 同 grep：glob 展开 + canonicalize 是同步阻塞 IO，隔离到阻塞线程池。
                let cwd = cx.cwd.clone();
                let roots = self.allowed_roots.clone();
                tokio::task::spawn_blocking(move || glob_search(&pattern, &cwd, &roots))
                    .await
                    .map_err(|e| ToolError::Failed(format!("glob 任务失败: {e}")))?
            }
        }
    }
}

/// 定位 edit 实际要用的匹配串，返回 `(needle, replacement)`；`None` = 匹配不上。
///
/// 先按原文试。不中且文件是 CRLF 时，把 `old`/`new` 的 LF 转成 CRLF 再试——
/// Windows 上这是头号失败源：模型给的跨行片段用 LF，文件里却是 CRLF，逐字符
/// 比较必然不等。
///
/// 关键是只转这两个串、不动文件其余部分：整体归一化换行会把无关行也改掉，
/// 一次 edit 变成整文件改写。
fn resolve_needle(content: &str, old: &str, new: &str) -> Option<(String, String)> {
    if content.contains(old) {
        return Some((old.to_string(), new.to_string()));
    }
    // 仅当文件确有 CRLF 且 old 里有独立的 LF 时才值得回退。
    if content.contains("\r\n") && old.contains('\n') && !old.contains("\r\n") {
        let crlf_old = old.replace('\n', "\r\n");
        if content.contains(&crlf_old) {
            return Some((crlf_old, new.replace('\n', "\r\n")));
        }
    }
    None
}

/// 匹配不上时给模型的诊断。
///
/// 只回「未找到」等于让它瞎猜，通常的结果是原样重试。模型看不到文件的确切字节，
/// 最常见的错因是凭记忆重构、缩进对不上，所以要指出方向。
fn no_match_hint(content: &str, old: &str) -> String {
    let mut msg = String::from("未在文件中找到 old_string，文件未改动。");

    // 空白折叠后能匹配 → 就是缩进/空白的问题，明说，别让它猜。
    if collapse_ws(content).contains(&collapse_ws(old)) {
        msg.push_str(
            "\n内容能对上，但**空白不同**（缩进宽度、空格与制表符、行尾空格）。\
             old_string 必须与文件逐字符一致，请先用 op=read 取回原文再照抄。",
        );
        return msg;
    }

    // 首行能定位 → 告诉它去读哪一段，省一次盲目 read 全文。
    if let Some(first) = old.lines().next().map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(n) = content.lines().position(|l| l.trim() == first) {
            msg.push_str(&format!(
                "\n首行「{first}」出现在第 {} 行附近，但后续内容不一致。\
                 建议 op=read 取回该处原文后再改。",
                n + 1
            ));
            return msg;
        }
    }

    msg.push_str("\n请先用 op=read 确认文件当前内容——它可能与你记忆中的不同。");
    msg
}

/// 把所有连续空白折叠成单个空格，用于「除了空白之外是否一致」的探测。
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 在文件或目录（递归）内按正则搜内容。返回 相对路径:行号:内容。
fn grep(pattern: &str, root: &Path) -> ToolResult<ToolOutput> {
    let re = regex::Regex::new(pattern).map_err(|e| ToolError::BadArgs(format!("正则非法: {e}")))?;
    let mut out = String::new();
    let mut count = 0usize;
    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        // 二进制/大文件跳过：读文本失败即跳过。
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                out.push_str(&format!("{}:{}:{}\n", path.display(), i + 1, line.trim()));
                count += 1;
                if count >= MAX_MATCHES {
                    out.push_str("…[匹配过多，已截断]\n");
                    return Ok(ToolOutput::ok(sanitize(&out)));
                }
            }
        }
    }
    if out.is_empty() {
        out.push_str("（无匹配）");
    }
    Ok(ToolOutput::ok(sanitize(&out)))
}

/// 按 glob 模式找文件，相对 cwd 展开；结果受 allowed_roots 约束。
fn glob_search(pattern: &str, cwd: &Path, allowed_roots: &[PathBuf]) -> ToolResult<ToolOutput> {
    // 相对模式以 cwd 为基准。
    let full = if Path::new(pattern).is_absolute() {
        pattern.to_string()
    } else {
        cwd.join(pattern).to_string_lossy().into_owned()
    };
    let paths = glob::glob(&full).map_err(|e| ToolError::BadArgs(format!("glob 模式非法: {e}")))?;
    let mut out = String::new();
    let mut count = 0usize;
    for entry in paths.flatten() {
        // 受 allowed_roots 约束：两边都 canonicalize 后前缀匹配（Windows \\?\ 归一）。
        let canon = entry.canonicalize().unwrap_or(entry);
        let allowed = allowed_roots.is_empty()
            || allowed_roots.iter().any(|r| {
                let rc = r.canonicalize().unwrap_or_else(|_| r.clone());
                canon.starts_with(&rc)
            });
        if !allowed {
            continue;
        }
        out.push_str(&format!("{}\n", canon.display()));
        count += 1;
        if count >= MAX_MATCHES {
            out.push_str("…[结果过多，已截断]\n");
            break;
        }
    }
    if out.is_empty() {
        out.push_str("（无匹配文件）");
    }
    Ok(ToolOutput::ok(sanitize(&out)))
}
