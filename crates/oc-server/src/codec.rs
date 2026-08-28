//! NDJSON 帧编解码（设计 §2.5）。每帧一行 JSON。
//!
//! 泛型于 `AsyncRead`/`AsyncWrite`，让上层逻辑与具体传输（unix socket /
//! 命名管道 / WS）解耦。

use oc_proto::Frame;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::{ServerError, ServerResult};

/// 帧读取器：按行读，解析为 [`Frame`]。
pub struct FrameReader<R> {
    inner: BufReader<R>,
    buf: String,
}

impl<R: tokio::io::AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader),
            buf: String::new(),
        }
    }

    /// 读下一帧。返回 `Ok(None)` 表示对端关闭。空行被跳过。
    pub async fn read_frame(&mut self) -> ServerResult<Option<Frame>> {
        loop {
            self.buf.clear();
            let n = self.inner.read_line(&mut self.buf).await?;
            if n == 0 {
                return Ok(None);
            }
            let line = self.buf.trim_end();
            if line.is_empty() {
                continue;
            }
            let frame = serde_json::from_str::<Frame>(line)
                .map_err(|e| ServerError::Codec(format!("解析帧失败: {e}")))?;
            return Ok(Some(frame));
        }
    }
}

/// 帧写入器：序列化为一行 JSON 追加 `\n`。
pub struct FrameWriter<W> {
    inner: W,
}

impl<W: tokio::io::AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { inner: writer }
    }

    pub async fn write_frame(&mut self, frame: &Frame) -> ServerResult<()> {
        let mut line = serde_json::to_string(frame)
            .map_err(|e| ServerError::Codec(format!("序列化帧失败: {e}")))?;
        line.push('\n');
        self.inner.write_all(line.as_bytes()).await?;
        self.inner.flush().await?;
        Ok(())
    }
}
