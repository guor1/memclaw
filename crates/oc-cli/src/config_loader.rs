//! 加载 `~/.oc/config.toml`（设计 §13.2）。
//!
//! 文件存在则解析并校验；不存在则用默认配置，并**不**自动写盘（由 `oc onboard`
//! 或用户手动创建）。校验失败直接报错，避免带病启动。

use anyhow::{Context, Result};
use oc_core::Config;

use crate::paths;

/// 加载配置。文件缺失时回退默认配置（内含 mock/anthropic 占位）。
pub fn load() -> Result<Config> {
    let path = paths::config_path()?;
    if !path.exists() {
        eprintln!(
            "[info] 未找到 {}，使用默认配置。可参考 config.example.toml 创建。",
            path.display()
        );
        return Ok(Config::default_local());
    }

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let cfg: Config = toml::from_str(&text)
        .with_context(|| format!("解析 {} 失败（TOML 格式错误）", path.display()))?;
    cfg.validate_shape()
        .map_err(|report| anyhow::anyhow!("配置校验失败:\n{report}"))?;
    Ok(cfg)
}
