//! 记忆系统纯策略（设计 §4.1–4.5）。
//!
//! **纯函数集合**：排名公式、trigger 预筛、provenance 分类、User model supersede、
//! standing intent 预筛。所有外部量（now、候选集、查询）作为参数传入；
//! SQL/向量执行在 oc-store，编排在 oc-server。
//!
//! M5：相关性用**词法**（关键词重合）算；向量语义检索留接口后补。

/// 记忆分层（设计 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// AGENTS/SOUL/USER 指令类，会话起始注入。
    Curated,
    /// 情节记忆，按需搜，从不自动注入。
    Episodic,
    /// 待办/意图，触发时注入。
    Prospective,
    /// 给人读的回顾（DREAMS.md）。
    Review,
}

/// 记忆来源（provenance，抗投毒，设计 §4.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Owner,
    Agent,
    Untrusted,
    System,
}

/// 写记忆的来源上下文，用于分类 origin。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteSource {
    /// 用户显式"记住…"。
    UserExplicit,
    /// 主会话 agent 推断。
    MainAgent,
    /// web_fetch/web_search 等外部内容。
    ExternalContent,
    /// cron/heartbeat/subagent 会话。
    BackgroundSession,
    /// 无法判定。
    Unknown,
}

/// provenance 分类（设计 §4.2）：**绝不默认 Owner**，无法判定 → 保守。
pub fn classify_origin(src: WriteSource) -> Origin {
    match src {
        WriteSource::UserExplicit => Origin::Owner,
        WriteSource::MainAgent => Origin::Agent,
        WriteSource::ExternalContent => Origin::Untrusted,
        // 后台会话不产生 Owner 候选；无法判定保守归 System/Untrusted。
        WriteSource::BackgroundSession => Origin::System,
        WriteSource::Unknown => Origin::Untrusted,
    }
}

/// 后台会话（cron/heartbeat/subagent）是否应产生持久记忆候选（设计 §4.2）。
pub fn produces_persistent_candidate(src: WriteSource) -> bool {
    !matches!(src, WriteSource::BackgroundSession)
}

/// 一条记忆候选（从 store 取回的字段映射而来）。
#[derive(Debug, Clone)]
pub struct MemCandidate {
    pub id: String,
    pub tier: Tier,
    pub origin: Origin,
    pub text: String,
    pub importance: f64,
    /// 最近使用时间（unix 秒）；None 用 created_at 兜底由调用方保证。
    pub last_used_secs: i64,
}

/// 排名结果。
#[derive(Debug, Clone)]
pub struct Ranked {
    pub id: String,
    pub score: f64,
}

/// 排名配置。
#[derive(Debug, Clone)]
pub struct RankCfg {
    /// 半衰期（秒）。默认 30 天。
    pub halflife_secs: f64,
}

impl Default for RankCfg {
    fn default() -> Self {
        Self { halflife_secs: 30.0 * 86400.0 }
    }
}

/// 词法相关性：查询词与文本的重合比例（0..1）。
///
/// 简单稳健：命中的查询词数 / 查询词总数。大小写不敏感。
pub fn lexical_relevance(text: &str, query_terms: &[String]) -> f64 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let lower = text.to_lowercase();
    let hits = query_terms
        .iter()
        .filter(|t| !t.is_empty() && lower.contains(&t.to_lowercase()))
        .count();
    hits as f64 / query_terms.len() as f64
}

/// 30 天半衰期因子：2^(-Δ/halflife)，Δ 为距今秒数（设计 §4.3）。
pub fn halflife_factor(now_secs: i64, last_used_secs: i64, halflife_secs: f64) -> f64 {
    let delta = (now_secs - last_used_secs).max(0) as f64;
    2f64.powf(-delta / halflife_secs)
}

/// Lane1 排名公式（设计 §4.3）：相关性 × 半衰期 × importance。
///
/// 纯函数：候选集 + 查询词 + now → 按 score 降序排列的结果。
pub fn rank(
    cands: &[MemCandidate],
    query_terms: &[String],
    now_secs: i64,
    cfg: &RankCfg,
) -> Vec<Ranked> {
    let mut out: Vec<Ranked> = cands
        .iter()
        .map(|c| {
            let rel = lexical_relevance(&c.text, query_terms);
            let hl = halflife_factor(now_secs, c.last_used_secs, cfg.halflife_secs);
            Ranked {
                id: c.id.clone(),
                score: rel * hl * c.importance,
            }
        })
        .collect();
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// trigger 注入预筛（设计 §4.3）。
///
/// 词法相关性 ≥ 阈值、**仅 curated tier**、每轮最多 `max` 条，
/// 且排除已注入过的（防召回环）。返回命中的记忆 id（按相关性降序）。
pub fn trigger_prefilter(
    msg: &str,
    cands: &[MemCandidate],
    query_terms: &[String],
    threshold: f64,
    max: usize,
) -> Vec<String> {
    let _ = (msg, threshold); // 词法模式下用命中判定；msg/threshold 留待向量预筛
    // 词法命中数：记忆文本包含多少个 query term。
    let hit_count = |text: &str| -> usize {
        let lower = text.to_lowercase();
        query_terms
            .iter()
            .filter(|t| !t.is_empty() && lower.contains(&t.to_lowercase()))
            .count()
    };
    let mut scored: Vec<(usize, &MemCandidate)> = cands
        .iter()
        .filter(|c| c.tier == Tier::Curated)
        .map(|c| (hit_count(&c.text), c))
        .filter(|(hits, _)| *hits >= 1) // 至少命中一个有意义的词
        .collect();
    // 命中数多的优先，其次 importance。
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.importance.partial_cmp(&a.1.importance).unwrap_or(std::cmp::Ordering::Equal))
    });
    scored.into_iter().take(max).map(|(_, c)| c.id.clone()).collect()
}

/// 自动注入的 tier 白名单（设计 §4.3：自动注入仅限 curated）。
pub fn is_auto_injectable(tier: Tier) -> bool {
    matches!(tier, Tier::Curated)
}

/// User model supersede（设计 §4.5）：新偏好就地替换矛盾项，不 append。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupersedePlan {
    /// 替换已有偏好（同 key）。
    Replace { existing_id: String },
    /// 全新偏好，新增。
    Add,
    /// 与已有完全相同，忽略。
    Ignore,
}

/// 一条用户偏好。
#[derive(Debug, Clone)]
pub struct Pref {
    pub id: String,
    /// 归一化的主题 key（如 "回复风格"）。
    pub key: String,
    pub value: String,
}

/// 判定新偏好该 replace / add / ignore（设计 §4.5）。
pub fn supersede(existing: &[Pref], incoming: &Pref) -> SupersedePlan {
    for e in existing {
        if e.key == incoming.key {
            return if e.value == incoming.value {
                SupersedePlan::Ignore
            } else {
                SupersedePlan::Replace { existing_id: e.id.clone() }
            };
        }
    }
    SupersedePlan::Add
}

/// 偏好主题词表：`(归一化 key, 触发词)`。命中任一触发词即归入该主题。
///
/// 词表**刻意小而明确**。见 [`extract_pref_key`] 的保守性说明。
/// 触发词一律小写（匹配前把输入也转小写）。
const PREF_TOPICS: &[(&str, &[&str])] = &[
    (
        "编辑器",
        &[
            "vs code", "vscode", "neovim", "nvim", "vim", "emacs", "sublime",
            "jetbrains", "intellij", "编辑器", "ide",
        ],
    ),
    (
        "操作系统",
        &[
            "windows", "macos", "mac os", "linux", "ubuntu", "debian",
            "操作系统", "系统是", "系统用",
        ],
    ),
    (
        "编程语言",
        &[
            "rust", "python", "java", "golang", "go 语言", "typescript",
            "javascript", "c++", "编程语言", "主力语言", "写代码用",
        ],
    ),
    (
        "回复风格",
        &["简洁", "详细", "回复风格", "别啰嗦", "长话短说", "说重点"],
    ),
    (
        "回复语言",
        &["中文", "英文", "english", "回复语言", "用中文", "用英文"],
    ),
    ("称呼", &["叫我", "称呼我", "我的名字"]),
];

/// 从自由文本抽偏好主题 key（设计 §4.5(e) 的前置步骤）。
///
/// [`supersede`] 需要 key 才能判「同主题冲突」，但用户说的是自由文本
/// （"我改用 Neovim 了"）。本函数把文本归到预置主题上，让
/// "我用 VS Code" 与 "我改用 Neovim" 落到同一个 key（"编辑器"）从而互相替换。
///
/// **保守**：只认词表内的明确主题；未命中返回 `None`，调用方应退回普通记忆写入
/// （append + 内容哈希去重），**不要**猜。因为 [`SupersedePlan::Replace`] 会删掉
/// 旧条目——误判两条无关记忆为「同主题」会真的丢信息，代价远高于漏判
/// （漏判只是多留一条冗余记忆）。
///
/// **纯词法**：不调模型。确定性（同输入必得同 key，可重现、可单测）、零延迟、
/// 零 token。代价是覆盖面有限；漏判的主题后续扩词表即可，或等向量语义（P2）。
///
/// 多主题同时命中时，返回**词表中靠前**的那个（词表顺序即优先级），保证确定性。
pub fn extract_pref_key(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    PREF_TOPICS
        .iter()
        .find(|(_, triggers)| triggers.iter().any(|t| lower.contains(t)))
        .map(|(key, _)| key.to_string())
}

/// 一条 standing intent（事件型待办）。
#[derive(Debug, Clone)]
pub struct StandingIntent {
    pub id: String,
    /// 词法触发关键词。
    pub keywords: Vec<String>,
}

/// standing intent 预筛（设计 §4.5）：入站消息命中任一关键词即触发。
///
/// anti-nagging（cooldown/budget/expiry）由 proactive 判定，这里只做词法命中。
pub fn intent_prefilter(msg: &str, intents: &[StandingIntent]) -> Vec<String> {
    let lower = msg.to_lowercase();
    intents
        .iter()
        .filter(|i| i.keywords.iter().any(|k| !k.is_empty() && lower.contains(&k.to_lowercase())))
        .map(|i| i.id.clone())
        .collect()
}

/// 显式记忆意图识别（设计 §4.1 写入路径）：用户说"记住…/别忘了…"等 → curated。
///
/// 纯词法：命中前缀触发词则剥离触发词、返回要记的正文。**保守**：只认明确的
/// 显式指令，不猜；未命中返回 None（走沉淀路径，不是 curated）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitMemory {
    /// 要记住的正文（已剥离触发词）。
    pub content: String,
}

/// 显式"记住"触发前缀（中英）。命中且正文非空 → 用户显式 curated 写入。
const REMEMBER_TRIGGERS: &[&str] = &[
    "记住", "记一下", "记下", "别忘了", "帮我记", "请记住",
    "remember that ", "remember ", "note that ", "please remember ",
];

/// 识别显式记忆意图（设计 §4.1）。命中返回剥离触发词后的正文。
///
/// 触发词只在**消息开头**（去除前导空白后）匹配，避免把"我记住了你说的"这类
/// 陈述误判为写入指令。正文剥离后去除前导标点/空白；为空则视为未命中。
pub fn detect_explicit_memory(msg: &str) -> Option<ExplicitMemory> {
    let trimmed = msg.trim_start();
    let lower = trimmed.to_lowercase();
    for trig in REMEMBER_TRIGGERS {
        if lower.starts_with(trig) {
            // 用字符数切分，兼容中英（trig 是 ASCII 或纯中文，字节前缀等价）。
            let rest = &trimmed[trig.len()..];
            let content = rest
                .trim_start_matches(|c: char| {
                    c.is_whitespace() || c == '：' || c == ':' || c == ',' || c == '，'
                })
                .trim()
                .to_string();
            if !content.is_empty() {
                return Some(ExplicitMemory { content });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, tier: Tier, origin: Origin, text: &str, imp: f64, last: i64) -> MemCandidate {
        MemCandidate { id: id.into(), tier, origin, text: text.into(), importance: imp, last_used_secs: last }
    }

    #[test]
    fn origin_never_defaults_to_owner() {
        assert_eq!(classify_origin(WriteSource::UserExplicit), Origin::Owner);
        assert_eq!(classify_origin(WriteSource::MainAgent), Origin::Agent);
        assert_eq!(classify_origin(WriteSource::ExternalContent), Origin::Untrusted);
        assert_eq!(classify_origin(WriteSource::Unknown), Origin::Untrusted);
        assert_ne!(classify_origin(WriteSource::BackgroundSession), Origin::Owner);
        assert!(!produces_persistent_candidate(WriteSource::BackgroundSession));
    }

    #[test]
    fn lexical_relevance_counts_hits() {
        let terms = vec!["车".into(), "保险".into()];
        assert_eq!(lexical_relevance("我的车保险到期了", &terms), 1.0);
        assert_eq!(lexical_relevance("我的车很好", &terms), 0.5);
        assert_eq!(lexical_relevance("今天天气不错", &terms), 0.0);
        assert_eq!(lexical_relevance("任意", &[]), 0.0);
    }

    #[test]
    fn halflife_decays_over_time() {
        let hl = 30.0 * 86400.0;
        assert!((halflife_factor(0, 0, hl) - 1.0).abs() < 1e-9); // 刚用过
        let thirty_days = 30 * 86400;
        assert!((halflife_factor(thirty_days, 0, hl) - 0.5).abs() < 1e-6); // 半衰
        assert!(halflife_factor(60 * 86400, 0, hl) < 0.26); // 两个半衰期
    }

    #[test]
    fn rank_orders_by_combined_score() {
        let now = 0;
        let terms = vec!["车".into()];
        let cands = vec![
            // 相关但很旧 → 半衰期压低
            cand("old", Tier::Episodic, Origin::Owner, "车", 1.0, -(60 * 86400)),
            // 相关且新鲜 → 分高
            cand("fresh", Tier::Episodic, Origin::Owner, "车", 1.0, 0),
            // 不相关 → 分 0
            cand("irrel", Tier::Episodic, Origin::Owner, "天气", 1.0, 0),
        ];
        let ranked = rank(&cands, &terms, now, &RankCfg::default());
        assert_eq!(ranked[0].id, "fresh");
        assert_eq!(ranked.last().unwrap().id, "irrel");
        assert!(ranked.last().unwrap().score.abs() < 1e-9);
    }

    #[test]
    fn trigger_only_curated_and_capped() {
        let terms = vec!["偏好".into()];
        let cands = vec![
            cand("c1", Tier::Curated, Origin::Owner, "用户偏好简洁", 1.0, 0),
            cand("c2", Tier::Curated, Origin::Owner, "另一条偏好设置", 1.0, 0),
            cand("c3", Tier::Curated, Origin::Owner, "第三条偏好", 1.0, 0),
            // episodic 不应被自动注入
            cand("e1", Tier::Episodic, Origin::Owner, "偏好偏好", 1.0, 0),
        ];
        let hits = trigger_prefilter("聊到偏好", &cands, &terms, 0.5, 2);
        assert_eq!(hits.len(), 2, "应受 max 限制");
        assert!(!hits.contains(&"e1".to_string()), "episodic 不应自动注入");
        assert!(!is_auto_injectable(Tier::Episodic));
        assert!(is_auto_injectable(Tier::Curated));
    }

    #[test]
    fn supersede_replaces_conflicting_pref() {
        let existing = vec![
            Pref { id: "p1".into(), key: "回复风格".into(), value: "详细".into() },
        ];
        // 同 key 不同值 → 替换
        let inc = Pref { id: "new".into(), key: "回复风格".into(), value: "简洁".into() };
        assert_eq!(supersede(&existing, &inc), SupersedePlan::Replace { existing_id: "p1".into() });
        // 同 key 同值 → 忽略
        let same = Pref { id: "new".into(), key: "回复风格".into(), value: "详细".into() };
        assert_eq!(supersede(&existing, &same), SupersedePlan::Ignore);
        // 新 key → 新增
        let novel = Pref { id: "new".into(), key: "语言".into(), value: "中文".into() };
        assert_eq!(supersede(&existing, &novel), SupersedePlan::Add);
    }

    #[test]
    fn extract_pref_key_matches_known_topics() {
        // 同主题的不同表述必须归到同一 key——这正是 supersede 能判冲突的前提。
        assert_eq!(extract_pref_key("我用 VS Code 写代码").as_deref(), Some("编辑器"));
        assert_eq!(extract_pref_key("我改用 Neovim 了").as_deref(), Some("编辑器"));
        // 大小写不敏感。
        assert_eq!(extract_pref_key("我用 VSCODE").as_deref(), Some("编辑器"));
        assert_eq!(extract_pref_key("我现在用 macOS").as_deref(), Some("操作系统"));
        assert_eq!(extract_pref_key("主力语言是 Rust").as_deref(), Some("编程语言"));
        assert_eq!(extract_pref_key("回复简洁一点").as_deref(), Some("回复风格"));
        assert_eq!(extract_pref_key("叫我老王").as_deref(), Some("称呼"));
    }

    #[test]
    fn extract_pref_key_returns_none_for_non_pref() {
        // 非偏好类内容不得误判——误判会让无关记忆互相覆盖而丢信息。
        assert_eq!(extract_pref_key("周三下午有例会"), None);
        assert_eq!(extract_pref_key("房东电话 138xxxx"), None);
        assert_eq!(extract_pref_key("出差要带转换插头"), None);
        assert_eq!(extract_pref_key(""), None);
    }

    #[test]
    fn extract_pref_key_is_deterministic_on_multi_hit() {
        // 多主题命中时按词表顺序取靠前的（编辑器 在 操作系统 之前），保证可重现。
        let k = extract_pref_key("我在 Windows 上用 VS Code");
        assert_eq!(k.as_deref(), Some("编辑器"));
        // 重复调用结果稳定。
        assert_eq!(extract_pref_key("我在 Windows 上用 VS Code"), k);
    }

    #[test]
    fn intent_prefilter_matches_keywords() {
        let intents = vec![
            StandingIntent { id: "i1".into(), keywords: vec!["周报".into()] },
            StandingIntent { id: "i2".into(), keywords: vec!["生日".into(), "礼物".into()] },
        ];
        let hits = intent_prefilter("帮我准备生日礼物", &intents);
        assert_eq!(hits, vec!["i2".to_string()]);
        assert!(intent_prefilter("今天写代码", &intents).is_empty());
    }

    #[test]
    fn explicit_memory_detection() {
        // 中文触发 + 剥离触发词/标点。
        assert_eq!(
            detect_explicit_memory("记住：我喜欢简洁的回复"),
            Some(ExplicitMemory { content: "我喜欢简洁的回复".into() })
        );
        assert_eq!(
            detect_explicit_memory("别忘了 我对花生过敏"),
            Some(ExplicitMemory { content: "我对花生过敏".into() })
        );
        // 英文触发。
        assert_eq!(
            detect_explicit_memory("remember that I use vim"),
            Some(ExplicitMemory { content: "I use vim".into() })
        );
        // 只在开头触发：陈述句不误判。
        assert_eq!(detect_explicit_memory("我记住了你说的话"), None);
        // 触发词后为空 → 未命中。
        assert_eq!(detect_explicit_memory("记住"), None);
        // 普通消息。
        assert_eq!(detect_explicit_memory("帮我搜索今日头条"), None);
    }
}
