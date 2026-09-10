//! `memory.text` 的 FTS5 索引编码（P2-4）。
//!
//! **问题**：候选检索走 `text LIKE '%词%'`，前缀通配让任何索引都用不上，只能全表扫。
//! 实测（10 万条、每条数百字）最慢一次查询 560ms，且随行数线性增长——它跑在读连接池上
//! （P2-1）不再阻塞写线程，但用户仍要为每条消息等这一下。
//!
//! **为什么不能直接用 FTS5 的现成分词器**：
//! - `unicode61`（计划书推荐的那个）把一整段中文当**一个** token。索引里存着
//!   `用户喜欢简洁的回复`，查 `简洁` 得零行——中文召回会静默归零。
//! - `trigram` 支持子串匹配，但只对 ≥3 字的词生效；而上层 `tokenize()` 刻意产出
//!   **2-gram**（覆盖中文双字词），于是几乎每个中文查询词都落不进索引。
//!
//! **本模块的做法**：不换分词器，而是**换存进去的东西**。把文本预先切成
//! 「相邻两字」的词流交给 `unicode61`（它只负责按空格切开我们给的 token），
//! 查询侧用**同一个函数**编码，于是索引天然是 LIKE 结果的超集。
//!
//! ```text
//! 文本  「简洁，别啰嗦」 → 简洁 洁 别 别啰 啰嗦     （窗口跨标点时只留字母数字）
//! 查询词「洁，别」       → 洁 AND 别                 → 命中
//! ```
//!
//! **索引不判定最终结果**：它只收窄候选集，SQL 里仍带原来的 `LIKE` 复核。
//! 因为 `detail=none` 不存位置信息，AND 只能保证「这些两字窗口都出现过」而非
//! 「它们相邻」——`cdxbcxab` 会被 `abcd` 的索引条件放进来，复核负责把它剔掉。
//! 这层复核顺带让「索引与表不同步」只会**少召回**、不会返回错的记忆。

/// 一个 token 的最大字符数。取 2 是为了对齐上层 `tokenize()` 的 2-gram 语义。
const WINDOW: usize = 2;

/// 把一段文本切成索引 token 流：**每对相邻字符**为一个窗口，窗口内只留字母数字
/// 并转小写；空窗口丢弃。结果去重后用空格连接，交给 `unicode61` 按空格切开。
///
/// 窗口**跨空白与标点**取，不先按分隔符切段。这一点是正确性的关键：若先按标点
/// 切段再取窗口，`简洁，别啰嗦` 只会得到 `简洁 / 别啰 / 啰嗦`，而查询词 `洁，别`
/// 编码出的 `洁`、`别` 就都不在索引里——本该命中的行会被静默漏掉。
/// 跨标点取窗口让「文本编码」与「查询词编码」在同一套规则下闭合。
pub fn encode_doc(text: &str) -> String {
    let mut tokens = windows(text);
    tokens.sort();
    tokens.dedup();
    tokens.join(" ")
}

/// 为一个查询词构造 MATCH 子表达式：它的所有窗口 token 取 **AND**。
///
/// 返回 `None` 表示该词无法用索引表达（整词内一个字母数字都没有，如纯 emoji）。
fn term_expr(term: &str) -> Option<String> {
    let mut tokens = windows(term);
    tokens.sort();
    tokens.dedup();
    if tokens.is_empty() {
        return None;
    }
    let anded = tokens
        .iter()
        // token 只含字母数字（`windows` 已过滤），故引号内无需再转义。
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" AND ");
    Some(format!("({anded})"))
}

/// 把一组查询词拼成整条 MATCH 表达式：词之间取 **OR**（与原 `LIKE ... OR ...` 同义）。
///
/// **任一词无法表达就整体返回 `None`**，调用方须退回全表 LIKE。不能只丢掉那个词：
/// OR 语义下，少一个词就等于少召回「只含该词」的那些行，而调用方无从得知。
pub fn match_expr(query_terms: &[String]) -> Option<String> {
    if query_terms.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(query_terms.len());
    for t in query_terms {
        parts.push(term_expr(t)?);
    }
    Some(parts.join(" OR "))
}

/// 取 `s` 的全部相邻字符窗口，逐窗口只留字母数字并小写。
///
/// 单字符输入按自身出一个 token（否则 1 字的输入永远编码不出东西）。
fn windows(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    if chars.len() < WINDOW {
        return fold(&chars).into_iter().collect();
    }
    chars.windows(WINDOW).filter_map(fold).collect()
}

/// 窗口内的字符归一：小写 + 只留字母数字。全被滤掉则 `None`。
///
/// 转小写用 `char::to_lowercase`（可能一对多，如 `İ`），与 `unicode61` 自己的
/// 大小写折叠不必逐字节一致——两侧都过这个函数，一致性由此保证。
fn fold(w: &[char]) -> Option<String> {
    let t: String = w
        .iter()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| c.is_alphanumeric())
        .collect();
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取回文档侧 token 集合，便于断言。
    fn doc_tokens(s: &str) -> Vec<String> {
        encode_doc(s).split(' ').map(str::to_string).filter(|t| !t.is_empty()).collect()
    }

    /// 本模块的**核心不变量**：查询词编码出的每个 token，只要该词真是文本的子串，
    /// 就必须出现在文本的 token 里。它成立，索引才是 LIKE 结果的超集（不漏召回）。
    ///
    /// 这条测试是编码规则的守门人——把 `windows` 改成「先按标点切段再取窗口」
    /// 这类看似无害的优化会当场变红。
    #[test]
    fn query_tokens_are_always_subset_of_doc_tokens() {
        let docs = [
            "用户喜欢简洁的回复",
            "简洁，别啰嗦",
            "我在 VS Code 里写 Rust",
            "表现是：程序在高并发下偶发失败",
            "清单里，供后续排查参考！",
            "busy_timeout 调高一点",
            "读书、写作与散步",
        ];
        for doc in docs {
            let have: std::collections::HashSet<String> = doc_tokens(doc).into_iter().collect();
            let chars: Vec<char> = doc.chars().collect();
            // 穷举该文本的所有子串（长度 2..=6）——每个都是「真子串」，
            // 故其编码必须被文本的 token 集合覆盖。
            for len in 2..=6usize {
                for start in 0..chars.len().saturating_sub(len - 1) {
                    let sub: String = chars[start..start + len].iter().collect();
                    for tok in windows(&sub) {
                        assert!(
                            have.contains(&tok),
                            "子串 {sub:?} 的 token {tok:?} 不在文本 {doc:?} 的索引里——会漏召回"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn windows_span_punctuation_and_whitespace() {
        // 跨标点的窗口退化成单字 token（标点被滤掉），两侧规则一致即可。
        assert_eq!(doc_tokens("简洁，别"), vec!["别", "洁", "简洁"]);
        // 跨空格同理。
        assert!(doc_tokens("VS Code").contains(&"vs".to_string()));
    }

    #[test]
    fn match_expr_ands_within_term_ors_across_terms() {
        let e = match_expr(&["简洁".into()]).expect("可表达");
        assert_eq!(e, "(\"简洁\")");
        let e = match_expr(&["简洁".into(), "回复".into()]).expect("可表达");
        assert_eq!(e, "(\"简洁\") OR (\"回复\")", "词间应为 OR");
        // 三字词切成两个窗口，词内取 AND。
        let e = match_expr(&["别啰嗦".into()]).expect("可表达");
        assert_eq!(e, "(\"别啰\" AND \"啰嗦\")");
    }

    #[test]
    fn case_and_width_folding_is_symmetric() {
        // 大小写：查询与文档折叠到同一 token。
        assert_eq!(term_expr("VSCode"), term_expr("vscode"));
        // 文档侧同样小写，故大写查询能命中小写文本。
        assert!(doc_tokens("VSCODE 配置").contains(&"vs".to_string()));
    }

    /// 无法表达的词必须让**整条**表达式为 None，逼调用方退回 LIKE。
    #[test]
    fn unrepresentable_term_forces_full_fallback() {
        assert_eq!(term_expr("😀😀"), None, "纯符号无字母数字，编码不出 token");
        assert_eq!(match_expr(&["😀😀".into()]), None);
        // 关键：混在可表达的词里也必须整体回落，否则「只含该词」的行会被漏掉。
        assert_eq!(
            match_expr(&["简洁".into(), "😀😀".into()]),
            None,
            "一个词表达不了就得整体回落，不能悄悄丢掉它"
        );
        // 空词表也无从构造（调用方另有「无查询词」的分支）。
        assert_eq!(match_expr(&[]), None);
    }

    #[test]
    fn single_char_input_still_yields_a_token() {
        assert_eq!(windows("中"), vec!["中".to_string()]);
        assert_eq!(windows("，"), Vec::<String>::new(), "纯标点无 token");
        assert_eq!(windows(""), Vec::<String>::new());
    }

    /// token 去重：同一窗口在长文本里出现多次只存一份。
    /// `detail=none` 不存词频/位置，排名在 oc-core 做，故去重无损失。
    #[test]
    fn doc_tokens_are_deduped_and_sorted() {
        let t = doc_tokens("啊啊啊啊");
        assert_eq!(t, vec!["啊啊"], "重复窗口应折叠成一个 token");
    }
}
