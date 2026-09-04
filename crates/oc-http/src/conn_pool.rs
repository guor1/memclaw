//! Connection pool over oc-server's NDJSON socket/pipe.
//!
//! Each pooled entry is a channel pair backed by reader/writer tasks, so callers
//! work in terms of `Frame` values and never touch the byte stream.

use std::sync::Arc;

use oc_proto::Frame;
use oc_server::TransportKind;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

use crate::error::{HttpError, HttpResult};

/// Bound on queued frames per direction. Small on purpose: backpressure should
/// reach the HTTP handler rather than buffering an unbounded event backlog.
const CHANNEL_CAP: usize = 32;

#[derive(Clone)]
pub struct ConnPool {
    inner: Arc<Mutex<Vec<NdjsonConn>>>,
    transport: TransportKind,
    max_idle: usize,
}

/// One logical connection to the daemon.
pub struct NdjsonConn {
    pub tx: mpsc::Sender<Frame>,
    pub rx: mpsc::Receiver<Frame>,
}

impl NdjsonConn {
    /// Whether both directions are still live. A pooled connection whose tasks
    /// have exited must not be handed out again.
    fn is_healthy(&self) -> bool {
        !self.tx.is_closed()
    }
}

impl ConnPool {
    pub fn new(transport: TransportKind, max_idle: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
            transport,
            max_idle,
        }
    }

    /// Take an idle connection, or open a new one.
    pub async fn acquire(&self) -> HttpResult<NdjsonConn> {
        loop {
            let pooled = {
                let mut idle = self.inner.lock().await;
                idle.pop()
            };
            match pooled {
                // Drop dead entries rather than returning them to a caller.
                Some(conn) if conn.is_healthy() => return Ok(conn),
                Some(_) => continue,
                None => break,
            }
        }
        connect(&self.transport).await
    }

    /// Return a connection for reuse. Unhealthy or surplus connections are dropped.
    pub async fn release(&self, conn: NdjsonConn) {
        if !conn.is_healthy() {
            return;
        }
        let mut idle = self.inner.lock().await;
        if idle.len() < self.max_idle {
            idle.push(conn);
        }
    }
}

/// Open a transport-appropriate stream and wire up frame codec tasks.
async fn connect(transport: &TransportKind) -> HttpResult<NdjsonConn> {
    match transport {
        #[cfg(unix)]
        TransportKind::Unix(path) => {
            let s = tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| HttpError::Connection(format!("connect to {path:?} failed: {e}")))?;
            Ok(spawn_codec(s))
        }
        #[cfg(windows)]
        TransportKind::Pipe(name) => {
            use tokio::net::windows::named_pipe::ClientOptions;
            let s = ClientOptions::new()
                .open(name)
                .map_err(|e| HttpError::Connection(format!("connect to {name} failed: {e}")))?;
            Ok(spawn_codec(s))
        }
        #[cfg(not(unix))]
        TransportKind::Unix(_) => Err(HttpError::Internal(
            "unix sockets are unavailable on this platform".into(),
        )),
        #[cfg(not(windows))]
        TransportKind::Pipe(_) => Err(HttpError::Internal(
            "named pipes are unavailable on this platform".into(),
        )),
    }
}

/// Split a stream into NDJSON reader/writer tasks.
///
/// Generic rather than boxed: a trait object cannot combine `AsyncRead` and
/// `AsyncWrite`, and monomorphizing here keeps both platform branches on one
/// code path.
fn spawn_codec<S>(stream: S) -> NdjsonConn
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let (tx_out, mut rx_out) = mpsc::channel::<Frame>(CHANNEL_CAP);
    let (tx_in, rx_in) = mpsc::channel::<Frame>(CHANNEL_CAP);

    // Writer: frames out as newline-delimited JSON.
    tokio::spawn(async move {
        let mut writer = write_half;
        while let Some(frame) = rx_out.recv().await {
            let Ok(json) = serde_json::to_string(&frame) else {
                tracing::warn!("frame serialize failed; closing writer");
                break;
            };
            if writer.write_all(json.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                break;
            }
        }
    });

    // Reader: one JSON frame per line.
    tokio::spawn(async move {
        let mut lines = BufReader::new(read_half).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Frame>(&line) {
                        Ok(frame) => {
                            if tx_in.send(frame).await.is_err() {
                                break;
                            }
                        }
                        // A single malformed line should not tear down the
                        // connection; skip it and keep reading.
                        Err(e) => tracing::warn!(error = %e, "skipping malformed frame"),
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "read error; closing reader");
                    break;
                }
            }
        }
    });

    NdjsonConn {
        tx: tx_out,
        rx: rx_in,
    }
}
