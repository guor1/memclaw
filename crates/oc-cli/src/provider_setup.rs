//! 从 Config 组装 provider + 会话参数（设计 §4.8 SecretRef 解引用在 server 侧）。
//!
//! M3：解引用第一个模型的 api_key；无 key 则回退到 mock provider（离线可用，
//! 便于本地演示与测试）。真实 provider 在有 key 时启用。

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use oc_core::config::{ApprovalMode as CfgApprovalMode, Config, Hosting, Provider as ProviderKind, SecretRef};
use oc_core::tool::ApprovalMode;
use oc_llm::mock::MockProvider;
use oc_llm::Provider;
use oc_server::tools_bridge::ToolExecutor;
use oc_server::SessionConfig;
use oc_tools::exec::ExecTool;
use oc_tools::file::FileTool;
use oc_tools::ToolRegistry;

/// 返回 (provider, 会话配置, 心跳间隔)。
pub fn build(cfg: &Config) -> Result<(Arc<dyn Provider>, SessionConfig, Duration)> {
    let model = cfg
        .models
        .first()
        .ok_or_else(|| anyhow::anyhow!("配置中没有模型"))?;

    // 空闲看门狗阈值据 hosting 区分。
    let idle = match model.hosting {
        Hosting::Cloud => cfg.watchdog.idle_cloud_secs,
        Hosting::SelfHosted => cfg.watchdog.idle_self_secs,
    };

    // 组装工具注册表（exec + file）。
    let tools = build_tools(cfg)?;

    let session_cfg = SessionConfig {
        model: model.model.clone(),
        system_prompt: Some(
            "你是 oc，一个长期陪伴用户的个人助手。你可以使用 exec 工具执行命令、\
             file 工具读写文件来完成任务。危险命令会先请求用户审批。"
                .to_string(),
        ),
        idle_timeout: Duration::from_secs(idle),
        run_timeout: if cfg.watchdog.run_timeout_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(cfg.watchdog.run_timeout_secs))
        },
        queue_cap: 16,
        tools: Some(tools),
        warn_secs: idle, // 警告阈值取空闲看门狗阈值
        abort_min_secs: cfg.watchdog.abort_min_secs,
    };

    // 解引用 api_key。
    let key = resolve_secret(&model.api_key);

    let provider: Arc<dyn Provider> = match key {
        Some(k) if !k.is_empty() => make_real(model.provider, k, model.base_url.clone()),
        _ => {
            eprintln!("[warn] 未找到 API key，回退到 mock provider（离线演示）");
            Arc::new(MockProvider::echo_text(
                "（mock 回复）你好，我是 oc。配置 API key 后可接入真实模型。",
            ))
        }
    };

    Ok((provider, session_cfg, Duration::from_secs(cfg.proactive.heartbeat_secs)))
}

/// 组装工具注册表：exec（审批门由 config 派生）+ file（限当前目录 + OC_HOME）。
fn build_tools(cfg: &Config) -> Result<ToolExecutor> {
    let mode = match cfg.tools.approval.mode {
        CfgApprovalMode::Prompt => ApprovalMode::Prompt,
        CfgApprovalMode::Allow => ApprovalMode::Allow,
        CfgApprovalMode::Deny => ApprovalMode::Deny,
    };
    let exec_timeout = Duration::from_secs(cfg.tools.exec_timeout_secs);

    // file 允许根：当前工作目录 + OC_HOME。空环境下退回当前目录。
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(home) = crate::paths::oc_home() {
        roots.push(home);
    }

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ExecTool::new(mode, exec_timeout)));
    registry.register(Arc::new(FileTool::new(roots)));

    // process 工具：后台移交 channel，接口另一端在 serve_with 接到台账。
    let (handoff_tx, handoff_rx) = tokio::sync::mpsc::unbounded_channel();
    registry.register(Arc::new(oc_tools::process::ProcessTool::new(handoff_tx)));

    Ok(ToolExecutor::new(Arc::new(registry)).with_handoff(handoff_rx))
}

fn resolve_secret(s: &SecretRef) -> Option<String> {
    match s {
        SecretRef::Inline(v) => Some(v.clone()),
        SecretRef::Env(name) => std::env::var(name).ok(),
        SecretRef::File(path) => std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()),
    }
}

fn make_real(kind: ProviderKind, key: String, base_url: Option<String>) -> Arc<dyn Provider> {
    match kind {
        #[cfg(feature = "provider-openai")]
        ProviderKind::Openai => Arc::new(oc_llm::openai::OpenAiProvider::new(key, base_url)),
        #[cfg(feature = "provider-anthropic")]
        ProviderKind::Anthropic => Arc::new(oc_llm::anthropic::AnthropicProvider::new(key, base_url)),
        // 未启用对应 feature 时回退 mock（不 panic）。
        #[allow(unreachable_patterns)]
        _ => {
            let _ = (key, base_url);
            Arc::new(MockProvider::echo_text("（provider feature 未启用，mock 回复）"))
        }
    }
}
