//! 磁盘布局定位（设计 §13.1）。`~/.oc/`。

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use directories::BaseDirs;

/// oc 根目录 `~/.oc/`。
pub fn oc_home() -> Result<PathBuf> {
    // 允许环境变量覆盖（测试 / 自定义部署）。
    if let Ok(dir) = std::env::var("OC_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let base = BaseDirs::new().ok_or_else(|| anyhow!("无法定位用户目录"))?;
    Ok(base.home_dir().join(".oc"))
}

/// 单库路径 `~/.oc/oc.sqlite`。
pub fn db_path() -> Result<PathBuf> {
    Ok(oc_home()?.join("oc.sqlite"))
}

/// 配置文件路径 `~/.oc/config.toml`。M2 起加载使用。
#[allow(dead_code)]
pub fn config_path() -> Result<PathBuf> {
    Ok(oc_home()?.join("config.toml"))
}

/// agent 工作区 `~/.oc/workspace`：会话 cwd 初值 + file/sys 允许根之一。
///
/// 固定位置，不可配——工作区就是 agent 的家，跟 OC_HOME 一起搬即可，
/// 没有第二个合理答案。要换位置就换 OC_HOME。
///
/// 这里**故意不看** `std::env::current_dir()`：工作区同时是允许根，跟随启动
/// 目录意味着在家目录跑 `oc serve` 就把 `~/.ssh` 交给了模型。
pub fn workspace() -> Result<PathBuf> {
    Ok(oc_home()?.join("workspace"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 工作区必须在 OC_HOME 内——它是 allowed_roots 之一，跑到外面等于
    /// 悄悄扩大文件访问范围。
    #[test]
    fn workspace_lives_under_oc_home() {
        let home = oc_home().expect("测试环境应能定位 home");
        assert_eq!(workspace().unwrap(), home.join("workspace"));
    }
}
