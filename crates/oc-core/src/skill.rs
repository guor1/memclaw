//! 技能：类型 + frontmatter 解析 + 门控 + 内容指纹（纯函数，无 IO）。
//!
//! 对应 BOARD.md `ROAD-1`，格式对齐 `docs/design/skill-format.md`。
//! 文件扫描与 IO 在 `oc-cli/src/skills_loader.rs`，这里只做纯策略。

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// 一个技能。
#[derive(Debug, Clone)]
pub struct Skill {
    /// frontmatter `name`（缺省回退叶子目录名）。展示用。
    pub name: String,
    /// 相对 `skills/` 的可路由 slug：裸包 = 目录名，scoped 包 = `@scope/name`。
    /// 门控（allowlist/denylist）按它匹配，与 `name` 解耦（skill-format.md §Slugs）。
    pub slug: String,
    pub description: String,
    /// 剥离 frontmatter 后的正文。
    pub body: String,
    /// 正文 sha256（hex）。变了触发模型重读。
    pub fingerprint: String,
    pub enabled: bool,
    /// metadata.openclaw.os；空 = 不限平台。
    pub os: Vec<String>,
}

/// frontmatter 里我们关心的字段。未知字段忽略（不 deny_unknown_fields）。
#[derive(Debug, Default, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    enabled: Option<bool>,
    metadata: Option<Metadata>,
}

#[derive(Debug, Default, Deserialize)]
struct Metadata {
    /// 同时接受 `metadata.openclaw` / `metadata.clawdbot` / `metadata.clawdis`。
    #[serde(alias = "clawdbot", alias = "clawdis", default)]
    openclaw: Option<Openclaw>,
}

#[derive(Debug, Default, Deserialize)]
struct Openclaw {
    #[serde(default)]
    os: Vec<String>,
}

/// 正文 sha256（hex 小写）。
pub fn fingerprint(body: &str) -> String {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    format!("{:x}", h.finalize())
}

/// 解析一个技能的原始 `SKILL.md` 内容。
///
/// `slug` 为可路由 slug（裸包或 `@scope/name`），`leaf` 为叶子目录名——frontmatter
/// 缺 `name` 时回退到 `leaf`（skill-format.md：`name` 应匹配父目录，即叶子目录）。
///
/// - 不以 `---` 开头 → 无 frontmatter，整篇为正文，其余字段取缺省。
/// - 有 frontmatter 但 YAML 非法 / 缺闭界 `---` → `None`（调用方跳过并 warn）。
pub fn parse_skill(slug: &str, leaf: &str, raw: &str) -> Option<Skill> {
    if !raw.starts_with("---") {
        // 不以 `---` 开头 → 无 frontmatter，整篇为正文。
        let body = raw.trim().to_string();
        return Some(Skill {
            name: leaf.to_string(),
            slug: slug.to_string(),
            description: String::new(),
            fingerprint: fingerprint(&body),
            body,
            enabled: true,
            os: vec![],
        });
    }
    // 以 `---` 开头：缺闭界 `---` / YAML 非法 → None。
    let (fm_str, rest) = extract_frontmatter(raw)?;
    let fm: SkillFrontmatter = serde_yaml::from_str(fm_str).ok()?;
    let name = fm.name.unwrap_or_else(|| leaf.to_string());
    let description = fm.description.unwrap_or_default();
    let enabled = fm.enabled.unwrap_or(true);
    let os = fm
        .metadata
        .and_then(|m| m.openclaw)
        .map(|o| o.os)
        .unwrap_or_default();
    let body = rest.trim().to_string();
    Some(Skill {
        name,
        slug: slug.to_string(),
        description,
        fingerprint: fingerprint(&body),
        body,
        enabled,
        os,
    })
}

/// 前置条件：`raw` 以 `---` 开头。返回 `(frontmatter 文本, 正文)`；
/// 缺闭界 `---` 时返回 `None`。
fn extract_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let s = raw.strip_prefix("---")?;
    // 允许 `---` 后紧跟换行。
    let s = s.strip_prefix('\n').or_else(|| s.strip_prefix("\r\n")).unwrap_or(s);
    let end = s.find("\n---")?;
    let (fm, rest) = s.split_at(end);
    let rest = rest.trim_start_matches("\n---");
    Some((fm, rest.trim_start_matches('\n')))
}

fn normalize_os(os: &str) -> &str {
    match os {
        "darwin" | "macos" => "macos",
        "windows" | "win32" => "windows",
        "linux" => "linux",
        other => other,
    }
}

/// 技能声明的 os 是否匹配宿主平台。空列表 = 不限。
pub fn os_matches(required: &[String], host_os: &str) -> bool {
    if required.is_empty() {
        return true;
    }
    let host = normalize_os(host_os);
    required.iter().any(|r| normalize_os(r) == host)
}

/// 门控：enabled + denylist（优先）+ allowlist + os。
pub fn gated_skills(
    skills: Vec<Skill>,
    allowlist: &[String],
    denylist: &[String],
    host_os: &str,
) -> Vec<Skill> {
    skills
        .into_iter()
        .filter(|s| s.enabled)
        .filter(|s| !denylist.iter().any(|d| d == &s.slug))
        .filter(|s| allowlist.is_empty() || allowlist.iter().any(|a| a == &s.slug))
        .filter(|s| os_matches(&s.os, host_os))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"---
name: todoist
description: Manage Todoist tasks.
enabled: true
metadata:
  openclaw:
    os:
      - darwin
---

# Body
do the thing
"#;

    const FLOW: &str = r#"---
name: todoist
description: Manage Todoist tasks.
metadata: { "openclaw": { "os": ["darwin", "linux"] } }
---

body
"#;

    const NO_FM: &str = "just a body, no frontmatter\n";

    #[test]
    fn parses_frontmatter_and_strips_body() {
        let s = parse_skill("todoist", "todoist", SAMPLE).unwrap();
        assert_eq!(s.name, "todoist");
        assert_eq!(s.slug, "todoist");
        assert_eq!(s.description, "Manage Todoist tasks.");
        assert!(s.enabled);
        assert_eq!(s.os, vec!["darwin".to_string()]);
        assert_eq!(s.body, "# Body\ndo the thing");
        assert_eq!(s.fingerprint, fingerprint("# Body\ndo the thing"));
    }

    #[test]
    fn flow_style_metadata_parses() {
        let s = parse_skill("todoist", "todoist", FLOW).unwrap();
        assert_eq!(s.os, vec!["darwin".to_string(), "linux".to_string()]);
    }

    /// scoped 包（`@publisher/slug`）的 slug 应是完整 scoped 路径，name 回退叶子目录名。
    #[test]
    fn scoped_slug_keeps_full_path_and_name_falls_back_to_leaf() {
        let raw = "---\ndescription: 自改进\n---\nbody";
        let s = parse_skill("@pskoett/self-improving-agent", "self-improving-agent", raw).unwrap();
        assert_eq!(s.slug, "@pskoett/self-improving-agent");
        assert_eq!(s.name, "self-improving-agent"); // frontmatter 无 name，回退叶子目录
        assert_eq!(s.description, "自改进");
    }

    #[test]
    fn no_frontmatter_means_whole_file_is_body() {
        let s = parse_skill("mydir", "mydir", NO_FM).unwrap();
        assert_eq!(s.name, "mydir");           // 叶子目录名兜底
        assert_eq!(s.slug, "mydir");
        assert_eq!(s.description, "");
        assert!(s.enabled);                     // enabled 缺省 true
        assert!(s.os.is_empty());
        assert_eq!(s.body, "just a body, no frontmatter");
    }

    #[test]
    fn malformed_frontmatter_is_none() {
        let bad = "---\nname: [unclosed\n---\nbody";
        assert!(parse_skill("x", "x", bad).is_none());
    }

    #[test]
    fn unterminated_frontmatter_is_none() {
        assert!(parse_skill("x", "x", "---\nname: broken").is_none());
    }

    #[test]
    fn plain_no_frontmatter_still_some() {
        let s = parse_skill("mydir", "mydir", "no dashes here").unwrap();
        assert_eq!(s.name, "mydir");
        assert_eq!(s.body, "no dashes here");
    }

    #[test]
    fn disabled_and_denylist_win() {
        let skills = vec![
            Skill { name: "a".into(), slug: "a".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: false, os: vec![] },
            Skill { name: "b".into(), slug: "b".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
            Skill { name: "c".into(), slug: "c".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
        ];
        // enabled:false 过滤掉 a；denylist 优先于 allowlist，过滤 c。
        let out = gated_skills(skills, &[], &["c".to_string()], "linux");
        assert_eq!(out.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["b"]);
    }

    #[test]
    fn allowlist_restricts() {
        let skills = vec![
            Skill { name: "a".into(), slug: "a".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
            Skill { name: "b".into(), slug: "b".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
        ];
        let out = gated_skills(skills, &["a".to_string()], &[], "linux");
        assert_eq!(out.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["a"]);
    }

    #[test]
    fn os_matches_darwin_and_macos_aliases() {
        assert!(os_matches(&["darwin".to_string()], "macos"));
        assert!(os_matches(&["macos".to_string()], "macos"));
        assert!(os_matches(&[], "linux"));                     // 空 = 不限
        assert!(!os_matches(&["darwin".to_string()], "windows"));
        let skills = vec![
            Skill { name: "mac-only".into(), slug: "mac-only".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec!["darwin".to_string()] },
            Skill { name: "any".into(), slug: "any".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
        ];
        let out = gated_skills(skills, &[], &[], "windows");
        assert_eq!(out.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["any"]);
    }

    /// `windows` / `win32` 都应归一成 `windows`（规范只保留这两个别名）。
    #[test]
    fn os_matches_windows_and_win32_aliases() {
        assert!(os_matches(&["windows".to_string()], "windows"));
        assert!(os_matches(&["win32".to_string()], "windows"));
        assert!(!os_matches(&["win32".to_string()], "linux"));
        // 未文档化的 "mac" 别名必须不再命中 macos。
        assert!(!os_matches(&["mac".to_string()], "macos"));
    }

    /// 门控按目录 slug 匹配，而非 frontmatter `name`。
    ///
    /// frontmatter `name` 是可读展示名，可能与目录名（slug）不同；allowlist/denylist
    /// 必须键在 slug 上，否则按展示名匹配不到真正要开关的技能目录。
    #[test]
    fn gating_keys_on_slug_not_frontmatter_name() {
        // slug = "pdf", frontmatter name = "PDF 生成器"。
        let pdf = Skill {
            name: "PDF 生成器".into(),
            slug: "pdf".into(),
            description: "".into(),
            body: "".into(),
            fingerprint: "".into(),
            enabled: true,
            os: vec![],
        };
        let todo = Skill {
            name: "待办".into(),
            slug: "todo".into(),
            description: "".into(),
            body: "".into(),
            fingerprint: "".into(),
            enabled: true,
            os: vec![],
        };

        // allowlist 填 slug "pdf" → 只保留 pdf（按 name 匹配会一个都不留）。
        let out = gated_skills(
            vec![pdf.clone(), todo.clone()],
            &["pdf".to_string()],
            &[],
            "linux",
        );
        assert_eq!(out.iter().map(|s| s.slug.as_str()).collect::<Vec<_>>(), vec!["pdf"]);

        // denylist 填 slug "pdf" → 踢掉 pdf（按 name 匹配会踢不掉）。
        let out = gated_skills(
            vec![pdf.clone(), todo.clone()],
            &[],
            &["pdf".to_string()],
            "linux",
        );
        assert_eq!(out.iter().map(|s| s.slug.as_str()).collect::<Vec<_>>(), vec!["todo"]);
    }

    #[test]
    fn fingerprint_is_sha256_hex() {
        let f = fingerprint("abc");
        assert_eq!(f.len(), 64);
        assert_eq!(f, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
