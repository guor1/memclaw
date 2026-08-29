//! sys 工具：会话级系统命令，Rust 原生实现（优先于 exec，确定性 + 跨平台）。
//!
//! - `pwd`：当前会话工作目录
//! - `cd`：切换会话工作目录（受 allowed_roots 约束；成功经 ToolOutput.new_cwd 回传）
//! - `now`：当前时间（unix 秒 + RFC3339 近似）
//!
//! cwd 状态不在工具里（工具是无状态 `Arc<dyn Tool>`）；`cd` 只回传新目录，
//! 由 executor 写回每会话状态。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ToolResult};
use crate::path_guard::resolve_dir_in_roots;
use crate::types::{ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

/// sys 工具。持有允许根目录（cd 约束用）。
pub struct SysTool {
    allowed_roots: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum SysArgs {
    /// 当前工作目录。
    Pwd,
    /// 切换工作目录。
    Cd { path: String },
    /// 当前时间。
    Now,
}

impl SysTool {
    pub fn new(allowed_roots: Vec<PathBuf>) -> Self {
        Self { allowed_roots }
    }
}

#[async_trait]
impl Tool for SysTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sys".to_string(),
            description: "系统命令（优先于 exec）。op=pwd（当前目录）|cd（切换目录）|now（当前时间）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["pwd", "cd", "now"] },
                    "path": { "type": "string", "description": "cd 的目标目录（可相对当前目录）" }
                },
                "required": ["op"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: false,
            timeout: std::time::Duration::from_secs(10),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: SysArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;

        match args {
            SysArgs::Pwd => Ok(ToolOutput::ok(cx.cwd.display().to_string())),
            SysArgs::Cd { path } => {
                let target = resolve_dir_in_roots(Path::new(&path), &cx.cwd, &self.allowed_roots)?;
                let mut out = ToolOutput::ok(format!("已切换到 {}", target.display()));
                out.new_cwd = Some(target);
                Ok(out)
            }
            SysArgs::Now => {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(ToolOutput::ok(format!("unix秒: {secs}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx_with_cwd(cwd: PathBuf) -> ToolCtx {
        let mut cx = ToolCtx::detached(CancellationToken::new());
        cx.cwd = cwd;
        cx
    }

    #[tokio::test]
    async fn pwd_returns_cwd() {
        let tmp = std::env::temp_dir();
        let tool = SysTool::new(vec![]);
        let out = tool
            .invoke(serde_json::json!({"op": "pwd"}), ctx_with_cwd(tmp.clone()))
            .await
            .unwrap();
        assert_eq!(out.content, tmp.display().to_string());
        assert!(out.new_cwd.is_none());
    }

    #[tokio::test]
    async fn cd_returns_new_cwd_within_roots() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let sub = tmp.join(format!("oc-sys-test-{}", std::process::id()));
        std::fs::create_dir_all(&sub).unwrap();

        let tool = SysTool::new(vec![tmp.clone()]);
        let out = tool
            .invoke(
                serde_json::json!({"op": "cd", "path": sub.to_string_lossy()}),
                ctx_with_cwd(tmp.clone()),
            )
            .await
            .unwrap();
        assert_eq!(out.new_cwd.as_ref().unwrap().canonicalize().unwrap(), sub.canonicalize().unwrap());

        std::fs::remove_dir_all(&sub).ok();
    }

    #[tokio::test]
    async fn cd_outside_roots_rejected() {
        // 允许根设为一个不含目标的子目录，cd 到 temp 根应被拒。
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let root = tmp.join(format!("oc-sys-root-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        let tool = SysTool::new(vec![root.clone()]);
        let r = tool
            .invoke(
                serde_json::json!({"op": "cd", "path": tmp.to_string_lossy()}),
                ctx_with_cwd(root.clone()),
            )
            .await;
        assert!(matches!(r, Err(ToolError::PathNotAllowed(_))));

        std::fs::remove_dir_all(&root).ok();
    }
}
