//! 配置类型、校验、SecretRef、ReloadKind（设计 §4.8、§13.2）。
//!
//! 解引用 SecretRef（读 env/文件）是 IO，在 server 做；core 只持类型 + 校验形状。

use std::path::PathBuf;

use garde::Validate;
use serde::{Deserialize, Serialize};

/// 顶层配置。分节：server / models / memory / proactive / tools / watchdog。
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct Config {
    #[garde(range(min = 1))]
    pub proto_version: u16,

    #[garde(dive)]
    pub server: ServerConfig,

    #[garde(length(min = 1), dive)]
    pub models: Vec<ModelConfig>,

    #[garde(dive)]
    pub memory: MemoryConfig,

    #[garde(dive)]
    pub proactive: ProactiveConfig,

    #[garde(dive)]
    pub tools: ToolsConfig,

    #[garde(dive)]
    pub watchdog: WatchdogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ServerConfig {
    #[garde(skip)]
    pub transport: Transport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Unix,
    Pipe,
    Ws,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ModelConfig {
    #[garde(length(min = 1))]
    pub alias: String,
    #[garde(skip)]
    pub provider: Provider,
    #[garde(length(min = 1))]
    pub model: String,
    #[garde(skip)]
    pub hosting: Hosting,
    /// API key，SecretRef 三态。
    #[garde(skip)]
    pub api_key: SecretRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Openai,
    Anthropic,
}

/// 托管方式，决定空闲看门狗阈值（cloud 120s / self 300s，见 §10.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hosting {
    Cloud,
    #[serde(rename = "self")]
    SelfHosted,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct MemoryConfig {
    #[garde(skip)]
    pub vec: bool,
    #[garde(range(min = 1))]
    pub halflife_days: u32,
    #[garde(range(min = 0.0, max = 1.0))]
    pub trigger_threshold: f32,
    #[garde(range(min = 1))]
    pub trigger_max_per_turn: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ProactiveConfig {
    #[garde(range(min = 1))]
    pub heartbeat_secs: u64,
    #[garde(range(min = 0))]
    pub intent_cooldown_secs: u64,
    #[garde(range(min = 1))]
    pub intent_budget: u32,
    #[garde(range(min = 1))]
    pub intent_expiry_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ToolsConfig {
    #[garde(range(min = 1))]
    pub exec_timeout_secs: u64,
    #[garde(dive)]
    pub approval: ApprovalConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ApprovalConfig {
    #[garde(skip)]
    pub mode: ApprovalMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Prompt,
    Allow,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct WatchdogConfig {
    #[garde(range(min = 1))]
    pub idle_cloud_secs: u64,
    #[garde(range(min = 1))]
    pub idle_self_secs: u64,
    /// run 墙钟上限，0 = 无限。
    #[garde(skip)]
    pub run_timeout_secs: u64,
    /// 卡死 abort 下限（见 §10.1，默认 300）。
    #[garde(range(min = 1))]
    pub abort_min_secs: u64,
}

/// Secret 引用三态（inline/env/file）。解引用是 IO，在 server 做。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretRef {
    Inline(String),
    Env(String),
    File(PathBuf),
}

/// 每个配置项的热更能力（设计 §13.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadKind {
    /// 可 ArcSwap 热更（如 trigger_threshold、anti-nagging）。
    Hot,
    /// 需重启（如 transport、db 路径）。
    RestartRequired,
}

impl Config {
    /// 校验配置形状（garde）。SecretRef 的解引用不在此处。
    pub fn validate_shape(&self) -> Result<(), garde::Report> {
        Validate::validate(self)
    }

    /// 单用户本地默认配置。
    pub fn default_local() -> Self {
        Self {
            proto_version: 1,
            server: ServerConfig {
                transport: if cfg!(windows) {
                    Transport::Pipe
                } else {
                    Transport::Unix
                },
            },
            models: vec![ModelConfig {
                alias: "default".to_string(),
                provider: Provider::Anthropic,
                model: "claude-opus-4-8".to_string(),
                hosting: Hosting::Cloud,
                api_key: SecretRef::Env("ANTHROPIC_API_KEY".to_string()),
            }],
            memory: MemoryConfig {
                vec: true,
                halflife_days: 30,
                trigger_threshold: 0.72,
                trigger_max_per_turn: 3,
            },
            proactive: ProactiveConfig {
                heartbeat_secs: 60,
                intent_cooldown_secs: 86_400,
                intent_budget: 3,
                intent_expiry_days: 90,
            },
            tools: ToolsConfig {
                exec_timeout_secs: 120,
                approval: ApprovalConfig {
                    mode: ApprovalMode::Prompt,
                },
            },
            watchdog: WatchdogConfig {
                idle_cloud_secs: 120,
                idle_self_secs: 300,
                run_timeout_secs: 0,
                abort_min_secs: 300,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_local_is_valid() {
        let cfg = Config::default_local();
        assert!(cfg.validate_shape().is_ok(), "default config must validate");
    }

    #[test]
    fn rejects_out_of_range_threshold() {
        let mut cfg = Config::default_local();
        cfg.memory.trigger_threshold = 1.5;
        assert!(cfg.validate_shape().is_err());
    }

    #[test]
    fn rejects_empty_models() {
        let mut cfg = Config::default_local();
        cfg.models.clear();
        assert!(cfg.validate_shape().is_err());
    }
}
