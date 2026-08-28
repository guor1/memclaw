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
