//! 手写 SSE 解析（设计 §5，省依赖）。
//!
//! 在 `bytes` 流上累积，按 `data:` 行拆分事件。纯字节处理，可单测。

/// SSE 累积缓冲：喂字节，吐出完整的 `data:` 负载行。
#[derive(Default)]
pub struct SseBuffer {
    buf: String,
}

impl SseBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，返回本次凑齐的所有 data 负载（去掉 `data:` 前缀，trim）。
    ///
    /// SSE 以空行分隔事件；一个事件可能有多行 `data:`。这里逐行解析：
    /// 每遇到一个 `data:` 行即产出一条负载（OpenAI/Anthropic 均单行 data）。
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();

        // 按 \n 切；保留最后不完整的一段在 buf。
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf[..pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=pos);

            if let Some(rest) = line.strip_prefix("data:") {
                out.push(rest.trim().to_string());
            }
            // 其余行（event: / id: / 空行 / 注释）忽略。
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_data_line() {
        let mut b = SseBuffer::new();
        let out = b.push(b"data: hello\n\n");
        assert_eq!(out, vec!["hello".to_string()]);
    }

    #[test]
    fn handles_split_across_chunks() {
        let mut b = SseBuffer::new();
        assert!(b.push(b"data: par").is_empty());
        let out = b.push(b"tial\n");
        assert_eq!(out, vec!["partial".to_string()]);
    }

    #[test]
    fn ignores_non_data_lines() {
        let mut b = SseBuffer::new();
        let out = b.push(b"event: message\r\ndata: x\r\n\r\n");
        assert_eq!(out, vec!["x".to_string()]);
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut b = SseBuffer::new();
        let out = b.push(b"data: a\n\ndata: b\n\n");
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }
}
