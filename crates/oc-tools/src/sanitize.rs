//! 结果净化（设计 §6.3）：截断超长输出、剥控制字符、限制注入体积。

/// 单次工具输出最大字节数（防上下文爆 + 防注入）。
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;

/// 净化工具输出。
pub fn sanitize(raw: &str) -> String {
    // 剥除除 \n \t 外的控制字符。
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();

    if cleaned.len() <= MAX_OUTPUT_BYTES {
        return cleaned;
    }

    // 截断：保留头尾，中间省略（尾部常含错误信息，头部含上下文）。
    let head = MAX_OUTPUT_BYTES * 2 / 3;
    let tail = MAX_OUTPUT_BYTES - head;
    let head_str = floor_char_boundary_slice(&cleaned, head);
    let tail_start = cleaned.len().saturating_sub(tail);
    let tail_str = ceil_char_boundary_slice(&cleaned, tail_start);
    format!(
        "{head_str}\n\n…[输出过长，已截断 {} 字节]…\n\n{tail_str}",
        cleaned.len() - head - tail
    )
}

/// 取 `[..n]`，但把 n 下调到字符边界，避免切碎多字节字符。
fn floor_char_boundary_slice(s: &str, mut n: usize) -> &str {
    if n >= s.len() {
        return s;
    }
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// 取 `[n..]`，但把 n 上调到字符边界。
fn ceil_char_boundary_slice(s: &str, mut n: usize) -> &str {
    if n >= s.len() {
        return "";
    }
    while n < s.len() && !s.is_char_boundary(n) {
        n += 1;
    }
    &s[n..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_untouched() {
        assert_eq!(sanitize("hello\nworld"), "hello\nworld");
    }

    #[test]
    fn strips_control_chars() {
        let s = sanitize("a\x00b\x07c");
        assert_eq!(s, "abc");
    }

    #[test]
    fn keeps_newline_tab() {
        assert_eq!(sanitize("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn truncates_long_output() {
        let long = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let out = sanitize(&long);
        assert!(out.len() < long.len());
        assert!(out.contains("已截断"));
    }

    #[test]
    fn truncation_respects_utf8() {
        // 全中文超长输出，截断不应 panic 且是合法 UTF-8。
        let long = "中".repeat(MAX_OUTPUT_BYTES);
        let out = sanitize(&long);
        assert!(out.contains("已截断"));
        // String 本身即保证合法 UTF-8，能走到这就没 panic。
    }
}
