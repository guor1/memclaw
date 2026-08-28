//! file 工具（设计 §6.1）：读/写/列文件。
//!
//! 路径策略：限制在允许的根目录内（防越权读写）。写操作由 server 记审计。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ToolResult};
use crate::sanitize::sanitize;
use crate::types::{ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

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
}

impl FileTool {
    pub fn new(allowed_roots: Vec<PathBuf>) -> Self {
        Self { allowed_roots }
    }

    /// 检查路径是否在允许范围（规范化后前缀匹配）。
    fn check(&self, path: &Path) -> ToolResult<PathBuf> {
        // 规范化：尽量 canonicalize；不存在的写目标退回其父目录判断。
        let candidate = if path.exists() {
            path.canonicalize()?
        } else if let Some(parent) = path.parent() {
            if parent.as_os_str().is_empty() {
                std::env::current_dir()?.join(path)
            } else {
                parent.canonicalize()?.join(path.file_name().unwrap_or_default())
            }
        } else {
            return Err(ToolError::PathNotAllowed(path.display().to_string()));
        };

        if self.allowed_roots.is_empty() {
            return Ok(candidate);
        }
        let ok = self
            .allowed_roots
            .iter()
            .any(|root| candidate.starts_with(root));
        if ok {
            Ok(candidate)
        } else {
            Err(ToolError::PathNotAllowed(candidate.display().to_string()))
        }
    }
}

#[async_trait]
impl Tool for FileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "file".to_string(),
            description: "读/写/列文件。op=read|write|list。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["read", "write", "list"] },
                    "path": { "type": "string" },
                    "content": { "type": "string", "description": "write 时的内容" }
                },
                "required": ["op", "path"]
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
                let p = self.check(Path::new(&path))?;
                cx.update(format!("读取 {}\n", p.display()));
                let content = tokio::fs::read_to_string(&p).await?;
                Ok(ToolOutput::ok(sanitize(&content)))
            }
            FileArgs::Write { path, content } => {
                let p = self.check(Path::new(&path))?;
                if let Some(parent) = p.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&p, content.as_bytes()).await?;
                cx.update(format!("写入 {} ({} 字节)\n", p.display(), content.len()));
                Ok(ToolOutput::ok(format!("已写入 {}", p.display())))
            }
            FileArgs::List { path } => {
                let p = self.check(Path::new(&path))?;
                let mut entries = tokio::fs::read_dir(&p).await?;
                let mut out = String::new();
                while let Some(e) = entries.next_entry().await? {
                    let ft = e.file_type().await?;
                    let mark = if ft.is_dir() { "/" } else { "" };
                    out.push_str(&format!("{}{}\n", e.file_name().to_string_lossy(), mark));
                }
                Ok(ToolOutput::ok(sanitize(&out)))
            }
        }
    }
}
