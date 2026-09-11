//! 技能加载（ROAD-1）：读 `~/.oc/skills/<name>/SKILL.md`（目录式）为 Skill 列表。
//!
//! 每个技能一个目录，`SKILL.md` 带 YAML frontmatter。缺目录/读失败返回空列表
//! （不阻塞启动）；旧平铺 `*.md` 忽略。渲染时由 oc-core::prompt 稳定排序。

use oc_core::config::SkillsConfig;
use oc_core::skill::{gated_skills, parse_skill, Skill};

use crate::paths;

/// 加载所有技能文档并做门控。失败静默返回空。
pub fn load(cfg: &SkillsConfig, host_os: &str) -> Vec<Skill> {
    let dir = match paths::oc_home() {
        Ok(h) => h.join("skills"),
        Err(_) => return Vec::new(),
    };
    load_from_dir(&dir, cfg, host_os)
}

/// 从指定 skills 目录加载（抽出以便测试注入临时目录）。
fn load_from_dir(dir: &std::path::Path, cfg: &SkillsConfig, host_os: &str) -> Vec<Skill> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue; // 旧平铺 *.md 直接忽略。
        }
        let raw = match read_skill_md(&path) {
            Some(r) => r,
            None => continue,
        };
        let dir_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        match parse_skill(dir_name, &raw) {
            Some(s) => out.push(s),
            None => tracing::warn!(path = %path.display(), "skill frontmatter 解析失败，跳过"),
        }
    }
    gated_skills(out, &cfg.allowlist, &cfg.denylist, host_os)
}

/// 读目录下的 `SKILL.md`（回退 `skill.md`）。
fn read_skill_md(dir: &std::path::Path) -> Option<String> {
    for name in ["SKILL.md", "skill.md"] {
        let p = dir.join(name);
        if let Ok(s) = std::fs::read_to_string(&p) {
            return Some(s);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oc-skill-test-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scans_directories_and_parses_frontmatter() {
        let base = tmp();
        let skills = base.join("skills");
        fs::create_dir_all(skills.join("todoist")).unwrap();
        fs::write(skills.join("todoist").join("SKILL.md"), "---\nname: todoist\ndescription: Manage todos\n---\nbody").unwrap();
        // 旧平铺 *.md 应被忽略。
        fs::write(skills.join("legacy.md"), "old flat skill").unwrap();
        // 坏 frontmatter 目录跳过。
        fs::create_dir_all(skills.join("broken")).unwrap();
        fs::write(skills.join("broken").join("SKILL.md"), "---\nname: [unclosed\n---\nbody").unwrap();

        let cfg = SkillsConfig::default();
        let out = load_from_dir(&skills, &cfg, "linux");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "todoist");
        fs::remove_dir_all(&base).ok();
    }
}
