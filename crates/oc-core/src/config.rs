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
    /// 自定义 API 基地址（OpenAI 兼容端点，如 DeepSeek）。None = 官方默认。
    #[garde(skip)]
    #[serde(default)]
    pub base_url: Option<String>,
    /// 上下文窗口（token）。None = 按 model 名查内置默认表，查不到用保守默认。
    /// 压缩预算据此派生（budget = window − reserve）。
    #[garde(skip)]
    #[serde(default)]
    pub context_window: Option<u32>,
    /// 单轮**输出**上限（token），即请求体里的 `max_tokens`。
    ///
    /// 与 `context_window`（输入侧预算）是两回事，别混：这一项管模型一轮能吐多长。
    /// `None` = 不发该字段，由服务端挑默认值。
    ///
    /// thinking 类模型要留意：reasoning 也算在这个预算里。服务端默认值往往只够
    /// 一段普通回复，模型把预算烧在推理上就会被硬截断，一个工具都调不出来。
    #[garde(skip)]
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

/// 保守默认上下文窗口（内置表与手填都缺时兜底）。
pub const DEFAULT_CONTEXT_WINDOW: u32 = 32_768;

/// 按模型名查内置默认上下文窗口（手填缺失时的兜底表）。
///
/// 匹配常见模型名子串；查不到返回 [`DEFAULT_CONTEXT_WINDOW`]。单用户可随时
/// 在 config 显式填 `context_window` 覆盖本表（手填优先）。
pub fn default_context_window(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    // 从具体到通用匹配。
    if m.contains("deepseek") {
        65_536
    } else if m.contains("claude") {
        200_000
    } else if m.contains("gpt-4o")
        || m.contains("gpt-4.1")
        || m.contains("o1")
        || m.contains("o3")
        || m.contains("gpt-4-turbo")
        || m.contains("gpt-4-1106")
    {
        // 注意：本分支必须在下面的裸 "gpt-4" 之前——否则 gpt-4o / gpt-4-turbo
        // 会被 "gpt-4" 抢先命中，错拿 8K 窗口。
        128_000
    } else if m.contains("gpt-4") {
        8_192
    } else if m.contains("gpt-3.5") {
        16_385
    } else if m.contains("kimi") || m.contains("moonshot") {
        128_000
    } else if m.contains("qwen") {
        131_072
    } else if m.contains("gemini") {
        1_000_000
    } else {
        DEFAULT_CONTEXT_WINDOW
    }
}

impl ModelConfig {
    /// 生效的上下文窗口：手填优先，否则查内置默认表。
    pub fn effective_context_window(&self) -> u32 {
        self.context_window
            .unwrap_or_else(|| default_context_window(&self.model))
    }
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
    /// standing intent 每轮最多注入条数（设计 §12.5 ≤3）。
    ///
    /// 与上面三项的分工：cooldown/budget/expiry 是**每条待办自己的**参数（落库在
    /// standing_intent 行上，上面三项仅作新建时的默认值）；本项是**每轮全局**上限，
    /// 防止一条消息同时命中多条待办时把上下文塞满。
    #[garde(range(min = 1))]
    #[serde(default = "default_intent_max_per_turn")]
    pub intent_max_per_turn: u32,
}

/// `intent_max_per_turn` 的 serde 默认（老配置文件缺该键时用）。
fn default_intent_max_per_turn() -> u32 {
    3
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
    /// 等待用户审批回执的上限（秒）；超时按**拒绝**处理。`0` = 不超时。
    ///
    /// `#[serde(default)]` 是必需的：现网 `~/.oc/config.toml` 都没有这一项，
    /// 缺省必须能加载，否则升级即打断所有已有配置。
    #[serde(default = "default_approval_timeout_secs")]
    #[garde(skip)]
    pub timeout_secs: u64,
}

/// 审批等待上限默认 120s。无人值守场景（cron / HTTP 网关）没有 TUI 响应审批，
/// 无上限会让 run 占着车道直到卡死诊断兜底（默认 360s，且语义是「run 病了」）。
fn default_approval_timeout_secs() -> u64 {
    120
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
                base_url: None,
                context_window: None,
                max_output_tokens: None,
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
                intent_max_per_turn: 3,
            },
            tools: ToolsConfig {
                exec_timeout_secs: 120,
                approval: ApprovalConfig {
                    mode: ApprovalMode::Prompt,
                    timeout_secs: default_approval_timeout_secs(),
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

    /// 现网 `~/.oc/config.toml` 里没有 `timeout_secs`（该项后加的）。缺省必须
    /// 能加载并取到默认值，否则升级会打断所有已有配置。
    #[test]
    fn approval_timeout_defaults_when_absent() {
        let cfg: ApprovalConfig = toml::from_str(r#"mode = "prompt""#).expect("旧配置应能加载");
        assert_eq!(cfg.mode, ApprovalMode::Prompt);
        assert_eq!(cfg.timeout_secs, 120, "缺省应取默认 120s，而非 0（0 = 不超时）");
    }

    #[test]
    fn approval_timeout_explicit_wins() {
        let cfg: ApprovalConfig =
            toml::from_str("mode = \"prompt\"\ntimeout_secs = 5").expect("显式值应能加载");
        assert_eq!(cfg.timeout_secs, 5);
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

    #[test]
    fn default_context_window_table() {
        assert_eq!(default_context_window("deepseek-chat"), 65_536);
        assert_eq!(default_context_window("deepseek-reasoner"), 65_536);
        assert_eq!(default_context_window("claude-opus-4-8"), 200_000);
        assert_eq!(default_context_window("gpt-4o"), 128_000);
        assert_eq!(default_context_window("gemini-2.0-flash"), 1_000_000);
        // 未知模型 → 保守默认。
        assert_eq!(default_context_window("some-unknown-model"), DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn effective_window_prefers_explicit() {
        let mut m = Config::default_local().models.remove(0);
        m.model = "deepseek-chat".to_string();
        // 未填 → 查表得 65536。
        m.context_window = None;
        assert_eq!(m.effective_context_window(), 65_536);
        // 手填 → 优先。
        m.context_window = Some(100_000);
        assert_eq!(m.effective_context_window(), 100_000);
    }

    /// `intent_max_per_turn` 缺失时回落到默认 3（设计 §12.5 ≤3）。
    ///
    /// P1-2 新增了该键；已存在的 config.toml 里没有它，若无 serde default 会解析
    /// 失败、daemon 起不来。TOML 层面的端到端回归在 oc-cli::config_loader（那里才
    /// 有 toml 依赖与真实加载路径），此处只锁默认值本身。
    #[test]
    fn intent_max_per_turn_defaults_to_three() {
        assert_eq!(default_intent_max_per_turn(), 3);
        assert_eq!(Config::default_local().proactive.intent_max_per_turn, 3);
    }
}
