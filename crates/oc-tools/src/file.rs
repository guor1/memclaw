//! file 工具（设计 §6.1）：读/写/列文件。
//!
//! 路径策略：限制在允许的根目录内（防越权读写）。写操作由 server 记审计。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ToolResult};
use crate::path_guard::resolve_in_roots;
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
    fn check(&self, path: &Path, cwd: &Path) -> ToolResult<PathBuf> {
        resolve_in_roots(path, cwd, &self.allowed_roots)
    }
}

#[async_trait]
impl Tool for FileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "file".to_string(),
            description: "文件操作（优先于 exec）。op=read|write|list|stat|head|tail|grep|glob。\
                路径可相对当前工作目录。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["read", "write", "list", "stat", "head", "tail", "grep", "glob"] },
                    "path": { "type": "string", "description": "文件/目录路径（read/write/list/stat/head/tail/grep 用）" },
                    "content": { "type": "string", "description": "write 时的内容" },
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
