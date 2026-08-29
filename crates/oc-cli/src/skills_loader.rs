//! 技能加载（设计 §4.4、§13.1）：读 `~/.oc/skills/*.md` 为 SkillBrief 列表。
//!
//! 每个 `.md` 文件是一个技能：文件名（去扩展名）为技能名，正文为 body。
//! 缺目录/读失败 → 返回空列表（不阻塞启动）。渲染时由 oc-core::prompt 稳定排序。

use oc_core::prompt::SkillBrief;

use crate::paths;

/// 加载所有技能文档。失败静默返回空。
pub fn load() -> Vec<SkillBrief> {
    let dir = match paths::oc_home() {
        Ok(h) => h.join("skills"),
        Err(_) => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(), // 无 skills 目录
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(body) if !body.trim().is_empty() => out.push(SkillBrief {
                name: name.to_string(),
                body: body.trim().to_string(),
            }),
            _ => {}
        }
    }
    out
}
