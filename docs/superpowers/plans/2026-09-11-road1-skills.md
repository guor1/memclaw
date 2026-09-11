# ROAD-1 Skills 完整形态 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `~/.oc/skills/*.md` 最简版升级为目录式 `SKILL.md` + YAML frontmatter + `<available_skills>` 按需注入 + `enabled`/config/os 门控，兼容 ClawHub 技能格式。

**Architecture:** 领域逻辑（frontmatter 解析、门控、sha256 指纹）落在 `oc-core`（纯函数无 IO）；文件扫描在 `oc-cli` 的 loader。`Skill` 类型进 prompt 时只渲染「索引列表」（name + description + fingerprint），正文不进 system prompt，模型用 `file read` 按需读。

**Tech Stack:** Rust；`serde_yaml`（frontmatter 解析）、`sha2`（内容指纹）。

## Global Constraints

- `oc-core` 保持「纯策略、无 IO」：解析与门控只收字符串/配置，不碰文件系统；文件 IO 全在 `oc-cli`。
- 目录式布局：`~/.oc/skills/<name>/SKILL.md`（`skill.md` 回退；`skills.md` 暂不支持）。目录名满足 `1..=64` 小写字母/数字/连字符。
- 旧平铺 `~/.oc/skills/*.md` **直接忽略**，不做降级兼容。
- frontmatter 解析失败 → 整目录跳过 + `tracing::warn`，不 panic、不阻塞启动。
- 内容指纹用 **sha256**（hex），非 FNV-1a。
- os 门控匹配需同时接受 `darwin`/`macos`（ClawHub 两种写法并存）、`windows`、`linux`；`os` 空 = 不限平台。
- 所有新增条目维持 `oc-core` 现有「缺键兼容」约定（`#[serde(default)]`）。

---

### Task 1: oc-core 加 `skill` 模块（类型 + 解析 + 指纹 + 门控）

**Files:**
- Create: `crates/oc-core/src/skill.rs`
- Modify: `crates/oc-core/src/lib.rs:8-20`（注册模块）
- Modify: `crates/oc-core/Cargo.toml`（加 serde_yaml + sha2）

**Interfaces:**
- Produces:
  - `pub struct Skill { pub name: String, pub description: String, pub body: String, pub fingerprint: String, pub enabled: bool, pub os: Vec<String> }`
  - `pub fn parse_skill(dir_name: &str, raw: &str) -> Option<Skill>`
  - `pub fn fingerprint(body: &str) -> String`（sha256 hex）
  - `pub fn gated_skills(skills: Vec<Skill>, allowlist: &[String], denylist: &[String], host_os: &str) -> Vec<Skill>`
  - `pub fn os_matches(required: &[String], host_os: &str) -> bool`

- [ ] **Step 1: 加依赖**

`crates/oc-core/Cargo.toml` 的 `[dependencies]` 加：

```toml
serde_yaml = "0.9"
sha2 = "0.10"
```

- [ ] **Step 2: 写失败测试** `crates/oc-core/src/skill.rs` 底部 `#[cfg(test)] mod tests`

```rust
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
    let s = parse_skill("todoist", SAMPLE).unwrap();
    assert_eq!(s.name, "todoist");
    assert_eq!(s.description, "Manage Todoist tasks.");
    assert!(s.enabled);
    assert_eq!(s.os, vec!["darwin".to_string()]);
    assert_eq!(s.body, "# Body\ndo the thing");
    assert_eq!(s.fingerprint, fingerprint("# Body\ndo the thing"));
}

#[test]
fn flow_style_metadata_parses() {
    let s = parse_skill("todoist", FLOW).unwrap();
    assert_eq!(s.os, vec!["darwin".to_string(), "linux".to_string()]);
}

#[test]
fn no_frontmatter_means_whole_file_is_body() {
    let s = parse_skill("mydir", NO_FM).unwrap();
    assert_eq!(s.name, "mydir");           // 目录名兜底
    assert_eq!(s.description, "");
    assert!(s.enabled);                     // enabled 缺省 true
    assert!(s.os.is_empty());
    assert_eq!(s.body, "just a body, no frontmatter");
}

#[test]
fn malformed_frontmatter_is_none() {
    let bad = "---\nname: [unclosed\n---\nbody";
    assert!(parse_skill("x", bad).is_none());
}

#[test]
fn disabled_and_denylist_win() {
    let skills = vec![
        Skill { name: "a".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: false, os: vec![] },
        Skill { name: "b".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
        Skill { name: "c".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
    ];
    // enabled:false 过滤掉 a；denylist 优先于 allowlist，过滤 c。
    let out = gated_skills(skills, &[], &["c".to_string()], "linux");
    assert_eq!(out.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["b"]);
}

#[test]
fn allowlist_restricts() {
    let skills = vec![
        Skill { name: "a".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
        Skill { name: "b".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
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
        Skill { name: "mac-only".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec!["darwin".to_string()] },
        Skill { name: "any".into(), description: "".into(), body: "".into(), fingerprint: "".into(), enabled: true, os: vec![] },
    ];
    let out = gated_skills(skills, &[], &[], "windows");
    assert_eq!(out.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["any"]);
}

#[test]
fn fingerprint_is_sha256_hex() {
    let f = fingerprint("abc");
    assert_eq!(f.len(), 64);
    assert_eq!(f, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test -p oc-core skill::`
Expected: 编译失败（`skill` 模块与函数不存在）。

- [ ] **Step 4: 写实现** `crates/oc-core/src/skill.rs`

```rust
//! 技能：类型 + frontmatter 解析 + 门控 + 内容指纹（纯函数，无 IO）。
//!
//! 对应 BOARD.md `ROAD-1`，格式对齐 `docs/design/skill-format.md`。
//! 文件扫描与 IO 在 `oc-cli/src/skills_loader.rs`，这里只做纯策略。

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// 一个技能。
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
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
/// - 不以 `---` 开头 → 无 frontmatter，整篇为正文，其余字段取缺省（目录名当 name）。
/// - 有 frontmatter 但 YAML 非法 / 缺闭界 `---` → `None`（调用方跳过并 warn）。
pub fn parse_skill(dir_name: &str, raw: &str) -> Option<Skill> {
    match extract_frontmatter(raw) {
        Some((fm_str, rest)) => {
            let fm: SkillFrontmatter = serde_yaml::from_str(fm_str).ok()?;
            let name = fm.name.unwrap_or_else(|| dir_name.to_string());
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
                description,
                fingerprint: fingerprint(&body),
                body,
                enabled,
                os,
            })
        }
        None => {
            let body = raw.trim().to_string();
            Some(Skill {
                name: dir_name.to_string(),
                description: String::new(),
                fingerprint: fingerprint(&body),
                body,
                enabled: true,
                os: vec![],
            })
        }
    }
}

/// 若以 `---` 开头，返回 `(frontmatter 文本, 正文)`；否则 `None`。
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
        "darwin" | "macos" | "mac" => "macos",
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
        .filter(|s| !denylist.iter().any(|d| d == &s.name))
        .filter(|s| allowlist.is_empty() || allowlist.iter().any(|a| a == &s.name))
        .filter(|s| os_matches(&s.os, host_os))
        .collect()
}
```

- [ ] **Step 5: 注册模块** `crates/oc-core/src/lib.rs`

在 `pub mod tool;`（第 18 行）之后加：

```rust
pub mod skill;
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo test -p oc-core skill::`
Expected: 9 个测试全 PASS。

- [ ] **Step 7: Commit**

```bash
git add crates/oc-core/src/skill.rs crates/oc-core/src/lib.rs crates/oc-core/Cargo.toml
git commit -m "feat(core): skill 模块（类型 + frontmatter 解析 + 门控 + sha256 指纹）"
```

---

### Task 2: `[skills]` 配置节

**Files:**
- Modify: `crates/oc-core/src/config.rs`（加 `SkillsConfig` + `Config.skills` 字段 + `default_local`）

**Interfaces:**
- Produces: `pub struct SkillsConfig { pub allowlist: Vec<String>, pub denylist: Vec<String> }`，`Config` 新增 `pub skills: SkillsConfig`（`#[serde(default)]`）。

- [ ] **Step 1: 写失败测试** `crates/oc-core/src/config.rs` 的 `mod tests`

```rust
/// 现网 config.toml 没有 [skills] 节，缺省必须能加载。
#[test]
fn skills_section_defaults_when_absent() {
    let cfg: Config = toml::from_str(r#"
proto_version = 1
[server]
transport = "pipe"
[[models]]
alias = "default"
provider = "openai"
model = "m"
hosting = "cloud"
api_key = { env = "K" }
"#).expect("旧配置应能加载");
    assert!(cfg.skills.allowlist.is_empty());
    assert!(cfg.skills.denylist.is_empty());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-core config::skills_section_defaults_when_absent`
Expected: 编译失败（`Config` 无 `skills` 字段）。

- [ ] **Step 3: 加 `SkillsConfig` 并接线**

在 `WatchdogConfig` 定义（约 259 行）之后加：

```rust
/// 技能门控配置（ROAD-1）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Validate)]
pub struct SkillsConfig {
    /// 非空则只加载列表内的技能名。
    #[garde(skip)]
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// 永不加载（优先于 allowlist）。
    #[garde(skip)]
    #[serde(default)]
    pub denylist: Vec<String>,
}
```

`Config` struct 的 `watchdog` 字段后加：

```rust
    #[garde(skip)]
    #[serde(default)]
    pub skills: SkillsConfig,
```

`Config::default_local()`（约 329 行 `watchdog: WatchdogConfig { ... },` 之后）加：

```rust
            skills: SkillsConfig::default(),
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p oc-core config::`
Expected: 原有 config 测试 + 新测试全 PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/oc-core/src/config.rs
git commit -m "feat(core): config 加 [skills] allowlist/denylist"
```

---

### Task 3: prompt 按需注入（索引列表，正文不注入）

**Files:**
- Modify: `crates/oc-core/src/prompt.rs:20-25`（`SkillBrief`→`Skill` 引用）、`:42`、`:162-170`（渲染改索引）
- Modify: `crates/oc-server/src/session.rs:44`（`Vec<oc_core::prompt::SkillBrief>`→`Vec<oc_core::skill::Skill>`）
- Modify: `crates/oc-server/src/run.rs:82`（同上）
- Modify: `crates/oc-server/tests/prompt_wiring.rs:77`（断言改为「描述注入、正文不注入」）

**Interfaces:**
- Consumes: `oc_core::skill::Skill`（Task 1）。
- Produces: `render_system_prompt` 的 `skills` 段输出 `<name> — <description> [fingerprint <hash>]` 索引，无正文。

- [ ] **Step 1: 写失败测试** `crates/oc-core/src/prompt.rs` 的 `mod tests`

```rust
fn skill(name: &str, desc: &str, body: &str) -> crate::skill::Skill {
    crate::skill::Skill {
        name: name.into(),
        description: desc.into(),
        body: body.into(),
        fingerprint: crate::skill::fingerprint(body),
        enabled: true,
        os: vec![],
    }
}

/// 技能正文不得进 system prompt，只有索引（名字 + 描述 + 指纹）。
#[test]
fn skills_render_as_index_not_body() {
    let skills = [skill("pdf", "生成 PDF", "BODY_MARKER_XYZ")];
    let p = PromptInputs {
        soul: "s",
        platform: "",
        model: "",
        provider: "",
        endpoint: None,
        bootstrap: &[],
        skills: &skills,
        tools: &[],
        now: "t",
    };
    let rendered = render_system_prompt(&p);
    assert!(rendered.stable_prefix.contains("pdf"), "应含技能名");
    assert!(rendered.stable_prefix.contains("生成 PDF"), "应含描述");
    assert!(
        !rendered.stable_prefix.contains("BODY_MARKER_XYZ"),
        "正文不得注入：{rendered:?}"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-core prompt::skills_render_as_index_not_body`
Expected: 编译失败（`SkillBrief` 不存在 / `body` 注入导致 BODY_MARKER 命中）。

- [ ] **Step 3: 改 `prompt.rs` 类型与渲染**

删除 `SkillBrief` 定义（20-25 行）。`PromptInputs.skills` 类型改为 `&'a [crate::skill::Skill]`。

「技能」段（162-170 行）替换为：

```rust
    // 3) 技能：只注入索引列表（名字 + 描述 + 指纹），正文不进提示词——模型用
    //    file 工具按需读 `~/.oc/skills/<name>/SKILL.md`。按名称排序保持确定性。
    if !inputs.skills.is_empty() {
        let mut skills: Vec<&Skill> = inputs.skills.iter().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        prefix.push_str("\n# 技能\n可用技能（正文不在本提示词内，用 file 工具 read `~/.oc/skills/<name>/SKILL.md` 按需读取；指纹变了要重读）：\n");
        for s in skills {
            let desc = if s.description.is_empty() { "" } else { &s.description };
            prefix.push_str(&format!("- {} — {} [fingerprint {}]\n", s.name, desc, s.fingerprint));
        }
    }
```

顶部 `use` 引入：在文件头加 `use crate::skill::Skill;`（或直接 `crate::skill::Skill`，二选一，保持一致即可）。

- [ ] **Step 4: 改 server 侧类型引用**

`crates/oc-server/src/session.rs:44`：
```rust
    pub skills: Vec<oc_core::skill::Skill>,
```
`crates/oc-server/src/run.rs:82`：
```rust
    pub skills: Vec<oc_core::skill::Skill>,
```

`crates/oc-server/tests/prompt_wiring.rs:77` 改为：

```rust
    c.skills = vec![oc_core::skill::Skill {
        name: "pdf".into(),
        description: "生成 PDF".into(),
        body: "正文不该进提示词".into(),
        fingerprint: oc_core::skill::fingerprint("正文不该进提示词"),
        enabled: true,
        os: vec![],
    }];
```

同文件下方断言 `skills_reach_model_request` 改为：

```rust
    assert!(system.contains("pdf"), "技能名应注入: {system}");
    assert!(!system.contains("正文不该进提示词"), "正文不得注入: {system}");
```

（测试函数名可保留 `skills_reach_model_request`，语义已是「技能索引到达模型」。）

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p oc-core prompt:: -p oc-server --test prompt_wiring`
Expected: 全 PASS。

- [ ] **Step 6: 全工作区编译确认**

Run: `cargo build`
Expected: 编译通过（无 `SkillBrief` 残留引用）。

- [ ] **Step 7: Commit**

```bash
git add crates/oc-core/src/prompt.rs crates/oc-server/src/session.rs crates/oc-server/src/run.rs crates/oc-server/tests/prompt_wiring.rs
git commit -m "feat(core): 技能按需注入，base prompt 只放索引不放正文"
```

---

### Task 4: skills_loader 目录式扫描 + 门控

**Files:**
- Modify: `crates/oc-cli/src/skills_loader.rs`（整体重写）

**Interfaces:**
- Consumes: `oc_core::skill::{parse_skill, gated_skills, Skill}`、`oc_core::config::SkillsConfig`（Task 1/2）。
- Produces: `pub fn load(cfg: &SkillsConfig, host_os: &str) -> Vec<Skill>`。

- [ ] **Step 1: 写失败测试** `crates/oc-cli/src/skills_loader.rs` 底部 `#[cfg(test)] mod tests`

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-cli skills_loader::`
Expected: 编译失败（`load_from_dir` / `SkillsConfig` 引用不存在）。

- [ ] **Step 3: 重写 loader**

```rust
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
```

注意：删除文件头旧的 `use oc_core::prompt::SkillBrief;` 与旧的 `pub fn load() -> Vec<SkillBrief>`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p oc-cli skills_loader::`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/oc-cli/src/skills_loader.rs
git commit -m "feat(cli): skills_loader 改目录式 SKILL.md 扫描 + 门控"
```

---

### Task 5: 接线 + onboard 模板 + file 工具提示

**Files:**
- Modify: `crates/oc-cli/src/provider_setup.rs:59`（`load()` → `load(&cfg.skills, host_os)`）
- Modify: `crates/oc-cli/src/onboard.rs`（`DEFAULT_CONFIG` 加 `[skills]`、`DEFAULT_SKILL` 改目录式 + 写 `skills/example/SKILL.md`）
- Modify: `crates/oc-tools/src/file.rs`（`spec()` 的 `description` 补技能路径提示）

**Interfaces:**
- Consumes: `oc_core::config::SkillsConfig`（Task 2）、`load(&SkillsConfig, &str)`（Task 4）。

- [ ] **Step 1: 接线 provider_setup**

`crates/oc-cli/src/provider_setup.rs:59` 的 `skills: crate::skills_loader::load(),` 改为：

```rust
        skills: crate::skills_loader::load(&cfg.skills, std::env::consts::OS),
```

- [ ] **Step 2: onboard 模板 + 示例技能**

`onboard.rs` 的 `DEFAULT_CONFIG`（约 49 行 `[watchdog]` 之后）加：

```toml
[skills]
allowlist = []
denylist = []
```

`DEFAULT_SKILL` 常量（约 81 行）改为：

```rust
const DEFAULT_SKILL: &str = r#"---
name: example
description: 示例技能，演示 SKILL.md 格式。删除或替换为你自己的技能。
enabled: true
---

# example — 示例技能

skills/ 下每个子目录是一个技能，子目录里的 `SKILL.md` 是技能本体。
`description` 会进 base prompt 的可用技能列表；正文在模型按需用
`file read` 读取时才进入上下文。删除本目录或替换为你自己的技能。
"#;
```

`run()` 里写示例技能的地方（约 97-109 行）改为写目录式：

```rust
    let example_dir = skills_dir.join("example");
    fs::create_dir_all(&example_dir)?;
    write_if_absent(&example_dir.join("SKILL.md"), DEFAULT_SKILL, &mut created)?;
```

（删除旧的 `write_if_absent(&skills_dir.join("example.md"), DEFAULT_SKILL, ...)` 行。）

- [ ] **Step 3: file 工具 description 补技能路径提示**

`crates/oc-tools/src/file.rs` 的 `spec()` 里 `description` 字符串末尾加一句：

```rust
                "技能正文在 ~/.oc/skills/<name>/SKILL.md（用 op=read 按需读取）。",
```

（加进现有 `"文件操作（优先于 exec）。op=read|write|..."` 的描述末尾。）

- [ ] **Step 4: 编译 + 测试确认**

Run: `cargo build -p oc-cli -p oc-tools && cargo test -p oc-cli`
Expected: 通过（onboard 模板测试 `onboard.rs:154` 校验 `[skills]` 能解析）。

- [ ] **Step 5: Commit**

```bash
git add crates/oc-cli/src/provider_setup.rs crates/oc-cli/src/onboard.rs crates/oc-tools/src/file.rs
git commit -m "feat(cli): 接线 skills 门控 + onboard 目录式示例 + file 工具提示"
```

---

### Task 6: 全量验证 + 收尾

**Files:**
- Modify: `BOARD.md`（ROAD-1 标记完成/更新现状）

- [ ] **Step 1: 全工作区测试**

Run: `cargo test --workspace`
Expected: 全 PASS（含新增 10+ 测试），clippy 无新增告警。

- [ ] **Step 2: clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: 零告警。

- [ ] **Step 3: 手工验证**

1. `oc onboard` 后确认 `~/.oc/skills/example/SKILL.md` 存在（目录式）。
2. 手放一个带 `metadata.openclaw.os: ["darwin"]` 的技能目录，`oc serve` 起后发消息，
   确认该技能（非 darwin 宿主）不出现在 base prompt 索引里。
3. 放一个正常技能，确认索引里是「名字 + 描述 + fingerprint」，正文不在；模型能
   `file read ~/.oc/skills/<name>/SKILL.md` 拿到正文。

- [ ] **Step 4: 更新 BOARD.md**

`ROAD-1` 行现状改为「已完成：目录式 SKILL.md + frontmatter + 按需注入 + enabled/config/os 门控；env/bins 门控留后续」。条目明细 `ROAD-1` 段落同步勾掉三项、标注「env/bins 门控未做」。

- [ ] **Step 5: Commit**

```bash
git add BOARD.md
git commit -m "docs: ROAD-1 Skills 完整形态完成"
```
