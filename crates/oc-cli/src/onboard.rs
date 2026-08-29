//! 交互式初始化（设计 §9、§13.1）。
//!
//! 生成 `~/.oc/` 骨架：config.toml + soul/{SOUL,USER,AGENTS,MEMORY}.md。
//! 已存在的文件不覆盖（幂等；保护用户已有配置）。

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::paths;

/// 默认 config.toml（DeepSeek OpenAI 兼容示例，对齐 config.example.toml）。
const DEFAULT_CONFIG: &str = r#"proto_version = 1

[server]
# unix（Linux/macOS）| pipe（Windows）| ws
transport = "pipe"

# ── 模型 ─────────────────────────────────────────────────────
# DeepSeek 是 OpenAI 兼容接口，provider 用 "openai"，指定 base_url 即可。
[[models]]
alias = "default"
provider = "openai"
model = "deepseek-chat"
hosting = "cloud"
base_url = "https://api.deepseek.com/v1"
api_key = { env = "DEEPSEEK_API_KEY" }

[memory]
vec = false
halflife_days = 30
trigger_threshold = 0.72
trigger_max_per_turn = 3

[proactive]
heartbeat_secs = 60
intent_cooldown_secs = 86400
intent_budget = 3
intent_expiry_days = 90

[tools]
exec_timeout_secs = 120
[tools.approval]
mode = "prompt"

[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#;

const DEFAULT_SOUL: &str = r#"# SOUL.md — oc 的人格

你是 oc，一个长期陪伴我的个人助手。

- 说话简洁、直接、务实；不寒暄、不铺垫。
- 用中文回复（代码/技术术语用英文）。
- 可以用工具执行命令、读写文件来完成任务；危险操作先请求我审批。
- 记住我的偏好并主动应用；不确定时先问。
"#;

const DEFAULT_USER: &str = r#"# USER.md — 用户偏好

（oc 会随对话就地更新这里的偏好条目。）
"#;

const DEFAULT_AGENTS: &str = r#"# AGENTS.md — 指令

（长期生效的工作方式约定放这里。）
"#;

const DEFAULT_MEMORY: &str = r#"# MEMORY.md — curated 核心记忆

（dreaming 巩固会重写这里；也可手工编辑。）
"#;

const DEFAULT_SKILL: &str = r#"# example — 示例技能

这是一个技能示例。skills/ 下每个 .md 文件是一个技能，文件名即技能名，
正文会在会话起始注入系统提示词，用来教 oc 做某类任务的固定流程。

删除本文件或替换为你自己的技能。
"#;

pub fn run() -> Result<()> {
    let home = paths::oc_home()?;
    fs::create_dir_all(&home).with_context(|| format!("创建 {} 失败", home.display()))?;

    let soul_dir = home.join("soul");
    fs::create_dir_all(&soul_dir)?;
    fs::create_dir_all(home.join("memory"))?;
    fs::create_dir_all(home.join("logs"))?;
    let skills_dir = home.join("skills");
    fs::create_dir_all(&skills_dir)?;

    let mut created = Vec::new();
    write_if_absent(&home.join("config.toml"), DEFAULT_CONFIG, &mut created)?;
    write_if_absent(&soul_dir.join("SOUL.md"), DEFAULT_SOUL, &mut created)?;
    write_if_absent(&soul_dir.join("USER.md"), DEFAULT_USER, &mut created)?;
    write_if_absent(&soul_dir.join("AGENTS.md"), DEFAULT_AGENTS, &mut created)?;
    write_if_absent(&soul_dir.join("MEMORY.md"), DEFAULT_MEMORY, &mut created)?;
    write_if_absent(&skills_dir.join("example.md"), DEFAULT_SKILL, &mut created)?;

    println!("oc 初始化完成：{}", home.display());
    if created.is_empty() {
        println!("（所有文件已存在，未覆盖）");
    } else {
        println!("已创建：");
        for f in &created {
            println!("  {f}");
        }
    }
    println!();
    println!("下一步：");
    println!("  1. 设置模型 API key，例如：export DEEPSEEK_API_KEY=sk-xxxx");
    println!("     （或编辑 {}）", home.join("config.toml").display());
    println!("  2. 启动 daemon：oc serve");
    println!("  3. 另开终端进入对话：oc");
    Ok(())
}

/// 仅当文件不存在时写入；记录已创建的相对名。
fn write_if_absent(path: &Path, content: &str, created: &mut Vec<String>) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, content).with_context(|| format!("写入 {} 失败", path.display()))?;
    created.push(
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    Ok(())
}
