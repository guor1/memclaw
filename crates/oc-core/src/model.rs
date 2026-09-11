//! 模型目录 / 别名 / failover（设计 §4.6）。纯逻辑。
//!
//! 无 IO、无状态；仅持有某一代不可变目录的引用做别名解析与 failover 选择。

use serde::{Deserialize, Serialize};

/// 托管方式，决定空闲看门狗阈值（cloud/self）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hosting {
    Cloud,
    #[serde(rename = "self")]
    SelfHosted,
}

/// 一个模型条目。
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub alias: String,
    pub provider: String,
    pub model: String,
    pub hosting: Hosting,
}

/// 模型目录（某一代快照）。
#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    entries: Vec<ModelEntry>,
}

impl ModelCatalog {
    pub fn new(entries: Vec<ModelEntry>) -> Self {
        Self { entries }
    }

    /// 按别名解析。
    pub fn resolve<'a>(&'a self, alias: &str) -> Option<&'a ModelEntry> {
        self.entries.iter().find(|e| e.alias == alias)
    }

    /// 默认条目（第一个）。
    pub fn default_entry(&self) -> Option<&ModelEntry> {
        self.entries.first()
    }

    /// failover：给定已试过的别名，返回下一个未试过的条目。
    pub fn failover<'a>(&'a self, tried: &[String]) -> Option<&'a ModelEntry> {
        self.entries.iter().find(|e| !tried.contains(&e.alias))
    }

    /// 空闲看门狗阈值（秒），据 hosting 区分（设计 §10.1）。
    pub fn idle_timeout_secs(entry: &ModelEntry, cloud: u64, self_hosted: u64) -> u64 {
        match entry.hosting {
            Hosting::Cloud => cloud,
            Hosting::SelfHosted => self_hosted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> ModelCatalog {
        ModelCatalog::new(vec![
            ModelEntry {
                alias: "a".into(),
                provider: "anthropic".into(),
                model: "m1".into(),
                hosting: Hosting::Cloud,
            },
            ModelEntry {
                alias: "b".into(),
                provider: "openai".into(),
                model: "m2".into(),
                hosting: Hosting::SelfHosted,
            },
        ])
    }

    #[test]
    fn resolves_and_defaults() {
        let c = cat();
        assert_eq!(c.resolve("b").unwrap().model, "m2");
        assert_eq!(c.default_entry().unwrap().alias, "a");
        assert!(c.resolve("nope").is_none());
    }

    #[test]
    fn failover_skips_tried() {
        let c = cat();
        let next = c.failover(&["a".to_string()]).unwrap();
        assert_eq!(next.alias, "b");
        assert!(c.failover(&["a".into(), "b".into()]).is_none());
    }

    #[test]
    fn idle_timeout_by_hosting() {
        let c = cat();
        assert_eq!(ModelCatalog::idle_timeout_secs(c.resolve("a").unwrap(), 120, 300), 120);
        assert_eq!(ModelCatalog::idle_timeout_secs(c.resolve("b").unwrap(), 120, 300), 300);
    }
}
