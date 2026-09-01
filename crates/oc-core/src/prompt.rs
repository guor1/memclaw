//! 系统提示词组装（设计 §4.4）。纯函数：相同输入 → 逐字节相同输出。
//!
//! **prompt cache 确定性排序**（M3 验收项）：所有可变集合在渲染前按稳定 key
//! 排序；易变量（时间戳）集中放在尾部，让前缀稳定可缓存。

/// 一条注入的记忆行（curated tier）。
#[derive(Debug, Clone)]
pub struct MemLine {
    pub key: String,
    pub text: String,
}

/// 一个工具规格（仅名称/描述参与 prompt）。
#[derive(Debug, Clone)]
pub struct ToolBrief {
    pub name: String,
    pub description: String,
}

/// 一个技能文档。
#[derive(Debug, Clone)]
pub struct SkillBrief {
    pub name: String,
    pub body: String,
}

/// 组装输入。
pub struct PromptInputs<'a> {
    /// SOUL.md 人格（原样置顶）。
    pub soul: &'a str,
    /// 运行环境描述（OS + shell），进稳定前缀。空串则跳过该节。
    /// 让模型知道 exec 工具的目标 shell，避免在 Windows 上写 Unix 语法。
    pub platform: &'a str,
    /// curated 记忆注入（有预算，调用方已截断）。
    pub bootstrap: &'a [MemLine],
    pub skills: &'a [SkillBrief],
    pub tools: &'a [ToolBrief],
    /// 易变量：当前时间（RFC3339 字符串），放尾部。
    pub now: &'a str,
}

/// 渲染结果：稳定前缀 + 易变尾部分离，便于 provider prompt cache。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPrompt {
    /// 稳定前缀（soul + 排序后的 tools/skills/memory）。可被缓存。
    pub stable_prefix: String,
    /// 易变尾部（时间等）。不参与前缀缓存。
    pub volatile_suffix: String,
}

impl RenderedPrompt {
    /// 完整拼接（发送给不支持前缀缓存的 provider）。
    pub fn full(&self) -> String {
        format!("{}\n{}", self.stable_prefix, self.volatile_suffix)
    }
}

/// 确定性组装系统提示词。
pub fn render_system_prompt(inputs: &PromptInputs) -> RenderedPrompt {
    let mut prefix = String::new();

    // 1) 人格置顶（原样）。
    prefix.push_str("# 人格\n");
    prefix.push_str(inputs.soul.trim());
    prefix.push('\n');

    // 1.5) 运行环境（稳定：OS + shell）。让模型据此选正确的命令语法。
    if !inputs.platform.trim().is_empty() {
        prefix.push_str("\n# 运行环境\n");
        prefix.push_str(inputs.platform.trim());
        prefix.push('\n');
    }

    // 2) 工具：按名称字典序排序（确定性）。
    if !inputs.tools.is_empty() {
        let mut tools: Vec<&ToolBrief> = inputs.tools.iter().collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        prefix.push_str("\n# 工具\n");
        for t in tools {
            prefix.push_str(&format!("- {}: {}\n", t.name, t.description));
        }
        // P1-5：引导优先用结构化工具（特别是 cron_add），避免退化去拼 shell 命令（跨平台易错、绕过校验）。
        // **定时/延时提醒用 cron_add**，不要用 shell 睡眠阻塞等待（会卡住整个 run、触发超时/loop detection）。
        prefix.push_str(
            "\n优先使用结构化工具完成任务：查看/切换目录用 sys（pwd/cd/now），\
             读写/检索文件用 file（read/write/list/stat/head/tail/grep/glob）。\
             **定时/延时提醒用 cron_add**（如「12:50 提醒我喝水」），\
             不要用 shell 睡眠（Start-Sleep / sleep / timeout）阻塞等待——那会卡住整个对话。\
             仅当这些工具都覆盖不到时才用 exec 执行 shell 命令。\n",
        );
    }

    // 3) 技能：按名称排序。
    if !inputs.skills.is_empty() {
        let mut skills: Vec<&SkillBrief> = inputs.skills.iter().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        prefix.push_str("\n# 技能\n");
        for s in skills {
            prefix.push_str(&format!("## {}\n{}\n", s.name, s.body.trim()));
        }
    }

    // 4) 记忆：按 key 排序（确定性）。
    if !inputs.bootstrap.is_empty() {
        let mut mem: Vec<&MemLine> = inputs.bootstrap.iter().collect();
        mem.sort_by(|a, b| a.key.cmp(&b.key));
        prefix.push_str("\n# 记忆\n");
        for m in mem {
            prefix.push_str(&format!("- {}\n", m.text));
        }
    }

    // 易变尾部：时间。
    let suffix = format!("# 当前时间\n{}", inputs.now);

    RenderedPrompt {
        stable_prefix: prefix,
        volatile_suffix: suffix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(now: &'a str, tools: &'a [ToolBrief], mem: &'a [MemLine]) -> PromptInputs<'a> {
        PromptInputs {
            soul: "你是 oc。",
            platform: "",
            bootstrap: mem,
            skills: &[],
            tools,
            now,
        }
    }

    #[test]
    fn stable_prefix_ignores_tool_order() {
        let t1 = vec![
            ToolBrief { name: "b".into(), description: "B".into() },
            ToolBrief { name: "a".into(), description: "A".into() },
        ];
        let t2 = vec![
            ToolBrief { name: "a".into(), description: "A".into() },
            ToolBrief { name: "b".into(), description: "B".into() },
        ];
        let p1 = render_system_prompt(&inputs("T", &t1, &[]));
        let p2 = render_system_prompt(&inputs("T", &t2, &[]));
        assert_eq!(p1.stable_prefix, p2.stable_prefix, "工具顺序不应影响稳定前缀");
    }

    #[test]
    fn time_isolated_in_suffix() {
        let p1 = render_system_prompt(&inputs("T1", &[], &[]));
        let p2 = render_system_prompt(&inputs("T2", &[], &[]));
        assert_eq!(p1.stable_prefix, p2.stable_prefix, "时间变化不应影响稳定前缀");
        assert_ne!(p1.volatile_suffix, p2.volatile_suffix);
    }

    #[test]
    fn platform_in_stable_prefix() {
        let with = PromptInputs {
            soul: "你是 oc。",
            platform: "OS: Windows；shell: cmd.exe（用 cmd 语法，勿用 Unix 语法）。",
            bootstrap: &[],
            skills: &[],
            tools: &[],
            now: "NOW",
        };
        let r = render_system_prompt(&with);
        assert!(r.stable_prefix.contains("# 运行环境"));
        assert!(r.stable_prefix.contains("cmd.exe"));
        // 平台是稳定信息，不应进易变尾部。
        assert!(!r.volatile_suffix.contains("cmd.exe"));
    }

    #[test]
    fn empty_platform_skips_section() {
        let r = render_system_prompt(&inputs("NOW", &[], &[]));
        assert!(!r.stable_prefix.contains("# 运行环境"));
    }

    #[test]
    fn deterministic_byte_for_byte() {
        let t = vec![ToolBrief { name: "x".into(), description: "X".into() }];
        let m = vec![MemLine { key: "k".into(), text: "记住 A".into() }];
        let a = render_system_prompt(&inputs("NOW", &t, &m));
        let b = render_system_prompt(&inputs("NOW", &t, &m));
        assert_eq!(a, b);
    }
}
