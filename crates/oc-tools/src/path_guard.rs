//! 路径解析与安全校验（file / sys 工具共享）。
//!
//! 相对路径以会话工作目录 `cwd` 为基准（不再用进程 current_dir），
//! 规范化后必须落在 `allowed_roots` 内（空 = 信任环境，不限制）。

use std::path::{Path, PathBuf};

use crate::error::{ToolError, ToolResult};

/// 用户主目录：Windows 用 `USERPROFILE`，否则 `HOME`。无法定位时返回 `None`。
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(h) = std::env::var_os("USERPROFILE") {
            if !h.is_empty() {
                return Some(PathBuf::from(h));
            }
        }
    }
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// 展开路径**起点**的 `~`：`~` → 主目录；`~/...`（或 Windows `~\...`）→ 主目录 + 余下。
///
/// 只处理开头的 `~`，路径中间/末尾的 `~` 保持原样。主目录无法定位时原样返回。
pub fn expand_home(path: &str) -> String {
    let Some(home) = home_dir() else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    if path == "~" {
        return home.into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        // '/' 分隔符在 Windows 下 `Path` 同样接受。
        return format!("{home}/{}", rest);
    }
    if let Some(rest) = path.strip_prefix("~\\") {
        return format!("{home}\\{}", rest);
    }
    path.to_string()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_prefix_only() {
        let home = home_dir().expect("测试环境应能定位主目录");
        let h = home.to_string_lossy().into_owned();

        assert_eq!(expand_home("~"), h);
        assert_eq!(expand_home("~/"), format!("{h}/"));
        assert_eq!(expand_home("~/.oc/skills/foo"), format!("{h}/.oc/skills/foo"));
        assert_eq!(expand_home("~\\x"), format!("{h}\\x"));

        // 中间的 `~` 不动。
        assert_eq!(expand_home("a/~/b"), "a/~/b".to_string());
        // 非 `~` 开头的普通路径原样返回。
        assert_eq!(expand_home("/etc/hosts"), "/etc/hosts".to_string());
        assert_eq!(expand_home("rel.txt"), "rel.txt".to_string());
    }
}

