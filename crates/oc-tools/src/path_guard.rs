//! 路径解析与安全校验（file / sys 工具共享）。
//!
//! 相对路径以会话工作目录 `cwd` 为基准（不再用进程 current_dir），
//! 规范化后必须落在 `allowed_roots` 内（空 = 信任环境，不限制）。

use std::path::{Path, PathBuf};

use crate::error::{ToolError, ToolResult};

/// 把 `path`（可能相对 `cwd`）解析为规范化绝对路径，并校验在 `allowed_roots` 内。
///
/// 不存在的路径（如 write/mkdir 目标）退回用其父目录做校验。
pub fn resolve_in_roots(path: &Path, cwd: &Path, allowed_roots: &[PathBuf]) -> ToolResult<PathBuf> {
    // 相对路径 → 以 cwd 为基准。
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    // 规范化：存在则 canonicalize；不存在退回父目录 canonicalize + 文件名。
    let candidate = if abs.exists() {
        abs.canonicalize()?
    } else if let Some(parent) = abs.parent() {
        if parent.as_os_str().is_empty() {
            abs.clone()
        } else if parent.exists() {
            parent.canonicalize()?.join(abs.file_name().unwrap_or_default())
        } else {
            return Err(ToolError::PathNotAllowed(abs.display().to_string()));
        }
    } else {
        return Err(ToolError::PathNotAllowed(abs.display().to_string()));
    };

    if allowed_roots.is_empty() {
        return Ok(candidate);
    }
    // 关键：candidate 已 canonicalize（Windows 下带 `\\?\` extended-length 前缀），
    // roots 若未归一则前缀匹配必失败。对每个 root 也 canonicalize 后再比。
    let ok = allowed_roots.iter().any(|root| {
        let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
        candidate.starts_with(&root_canon)
    });
    if ok {
        Ok(candidate)
    } else {
        Err(ToolError::PathNotAllowed(candidate.display().to_string()))
    }
}

/// 校验一个必须存在的目录（用于 cd）：解析 + 校验根 + 确认是目录。
pub fn resolve_dir_in_roots(path: &Path, cwd: &Path, allowed_roots: &[PathBuf]) -> ToolResult<PathBuf> {
    let resolved = resolve_in_roots(path, cwd, allowed_roots)?;
    if !resolved.is_dir() {
        return Err(ToolError::BadArgs(format!("不是目录: {}", resolved.display())));
    }
    Ok(resolved)
}
