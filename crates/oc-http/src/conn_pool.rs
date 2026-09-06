//! Connection pool over oc-server's NDJSON socket/pipe.
//!
//! Each pooled entry is a channel pair backed by reader/writer tasks, so callers
//! work in terms of `Frame` values and never touch the byte stream.
//!
//! Two separate bounds:
//! - `max_conns` caps **live** connections (idle + checked out) via a semaphore.
//!   Each connection costs 2 tokio tasks and 2 bounded channels on this side plus
//!   a full connection handler on the daemon side, so this is the bound that
//!   matters under load.
//! - `max_idle` caps how many *idle* connections are kept for reuse; surplus ones
//!   are dropped on release.

use std::sync::Arc;
use std::time::Duration;

use oc_proto::Frame;
use oc_server::TransportKind;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore};

use crate::error::{HttpError, HttpResult};

/// Bound on queued frames per direction. Small on purpose: backpressure should
/// reach the HTTP handler rather than buffering an unbounded event backlog.
const CHANNEL_CAP: usize = 32;

/// How long [`ConnPool::acquire`] waits for a permit before giving up with
/// [`HttpError::Busy`].
///
/// A short bounded wait rather than either extreme: failing instantly would
/// reject requests that only needed to wait out a fast turn, while waiting
/// forever reproduces the hang this bound exists to prevent (SSE streams hold
/// their permit for the whole stream, so permits can stay taken for minutes).
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// Windows named-pipe `ERROR_PIPE_BUSY`: every instance is currently taken.
///
/// Not a real failure — the server creates the next instance only after the
/// previous one is connected, so concurrent clients routinely see this for a
/// moment. Retrying is the documented way to handle it.
#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

/// How long to keep retrying [`ERROR_PIPE_BUSY`] before treating it as a real
/// connection failure. Comfortably longer than the server needs to loop back
/// around to `accept`, yet short enough that a genuinely dead daemon still
/// fails well inside [`ACQUIRE_TIMEOUT`].
#[cfg(windows)]
const PIPE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(windows)]
const PIPE_BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone)]
pub struct ConnPool {
    inner: Arc<Mutex<Vec<NdjsonConn>>>,
    transport: TransportKind,
    max_idle: usize,
    max_conns: usize,
    /// One permit per live connection. Held by [`NdjsonConn`], so it is returned
    /// on drop — this covers the streaming path, which never calls `release`.
    permits: Arc<Semaphore>,
}

/// One logical connection to the daemon.
pub struct NdjsonConn {
    pub tx: mpsc::Sender<Frame>,
    pub rx: mpsc::Receiver<Frame>,
    /// Keeps a `max_conns` slot reserved for as long as this connection exists.
    ///
    /// `None` only for connections built outside a pool (tests). Pooled idle
    /// connections keep their permit: they are live connections and must count
    /// against the bound, otherwise `max_conns` would only limit *concurrent*
    /// use and the daemon-side handler count could still grow unbounded.
    _permit: Option<OwnedSemaphorePermit>,
}

impl NdjsonConn {
    /// Whether both directions are still live. A pooled connection whose tasks
    /// have exited must not be handed out again.
    fn is_healthy(&self) -> bool {
        !self.tx.is_closed()
    }
}

impl ConnPool {
    /// `max_conns` bounds live connections; `max_idle` bounds those kept for reuse.
    /// `max_idle` is clamped to `max_conns` — keeping more idle than the hard cap
    /// allows is not representable.
    pub fn new(transport: TransportKind, max_idle: usize, max_conns: usize) -> Self {
        let max_conns = max_conns.max(1);
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
            transport,
            max_idle: max_idle.min(max_conns),
            max_conns,
            permits: Arc::new(Semaphore::new(max_conns)),
        }
    }

    /// Take an idle connection, or open a new one.
    ///
    /// Returns [`HttpError::Busy`] if no permit frees up within
    /// [`ACQUIRE_TIMEOUT`]. Reusing an idle connection inherits its permit, so
    /// only genuinely new connections wait.
    pub async fn acquire(&self) -> HttpResult<NdjsonConn> {
        loop {
            let pooled = {
                let mut idle = self.inner.lock().await;
                idle.pop()
            };
            match pooled {
                // Reuse: the permit travels with the connection, nothing to take.
                Some(conn) if conn.is_healthy() => return Ok(conn),
                // Dead entry: dropping it frees its permit for the new connection
                // below, so this is not a leak.
                Some(_) => continue,
                None => break,
            }
        }

        let permit = tokio::time::timeout(ACQUIRE_TIMEOUT, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| {
                HttpError::Busy(format!(
                    "no daemon connection available within {}s; \
                     too many concurrent requests (limit {})",
                    ACQUIRE_TIMEOUT.as_secs(),
                    self.max_conns,
                ))
            })?
            // Only errors if the semaphore was closed, which never happens here.
            .map_err(|_| HttpError::Internal("connection limiter closed".into()))?;

        let mut conn = connect(&self.transport).await?;
        conn._permit = Some(permit);
        Ok(conn)
    }

    /// Live connections currently allowed to be created. Exposed for tests and
    /// diagnostics.
    pub fn available_permits(&self) -> usize {
        self.permits.available_permits()
    }

    /// Return a connection for reuse. Unhealthy or surplus connections are dropped
    /// (which frees their permit).
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

            // `ERROR_PIPE_BUSY` 必须重试，不能当失败。
            //
            // server 的 accept 循环一次只备**一个**管道实例：取出当前实例等连接，
            // 连上之后才创建下一个（见 oc-server/src/transport.rs `Listener::accept`）。
            // 于是并发连接时，后到的客户端可能撞上"所有管道范例都在使用中"
            // （os error 231）——这不是真的连不上，只是下一个实例还没建好。
            //
            // Windows 对此的标准做法就是等一下再试。不重试的表现是：`oc http`
            // 并发请求随机 500，而 daemon 侧毫无异常日志。
            let deadline = std::time::Instant::now() + PIPE_BUSY_TIMEOUT;
            loop {
                match ClientOptions::new().open(name) {
                    Ok(s) => return Ok(spawn_codec(s)),
                    Err(e)
                        if e.raw_os_error() == Some(ERROR_PIPE_BUSY)
                            && std::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(PIPE_BUSY_RETRY_INTERVAL).await;
                    }
                    Err(e) => {
                        return Err(HttpError::Connection(format!(
                            "connect to {name} failed: {e}"
                        )))
                    }
                }
            }
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
        // Filled in by `acquire`; `connect` itself has no pool context.
        _permit: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool whose transport never resolves to a real daemon. Enough to exercise
    /// the limiter, which runs before any connect attempt.
    fn pool(max_idle: usize, max_conns: usize) -> ConnPool {
        ConnPool::new(TransportKind::Pipe(r"\\.\pipe\oc-test-nonexistent".into()), max_idle, max_conns)
    }

    #[test]
    fn max_idle_is_clamped_to_max_conns() {
        let p = pool(64, 4);
        assert_eq!(p.max_idle, 4, "cannot keep more idle than the hard cap");
        assert_eq!(p.max_conns, 4);
    }

    #[test]
    fn max_conns_zero_is_raised_to_one() {
        // A pool that can never hand out a connection would deadlock every request.
        assert_eq!(pool(0, 0).max_conns, 1);
    }

    #[tokio::test]
    async fn permits_start_at_max_conns() {
        assert_eq!(pool(2, 3).available_permits(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_times_out_as_busy_when_permits_are_exhausted() {
        let p = pool(1, 1);
        // Take the only permit and hold it, standing in for an in-flight request
        // (or an open SSE stream).
        let held = p.permits.clone().acquire_owned().await.unwrap();
        assert_eq!(p.available_permits(), 0);

        // `NdjsonConn` is not Debug, so match rather than `expect_err`.
        match p.acquire().await {
            Err(HttpError::Busy(_)) => {}
            Err(other) => panic!("expected Busy, got {other:?}"),
            Ok(_) => panic!("must not hand out a connection when permits are gone"),
        }

        drop(held);
        assert_eq!(p.available_permits(), 1, "permit returns on drop");
    }

    #[tokio::test]
    async fn dropping_a_connection_returns_its_permit() {
        let p = pool(1, 2);
        let permit = p.permits.clone().acquire_owned().await.unwrap();
        let (tx, _rx_unused) = mpsc::channel::<Frame>(1);
        let (_tx_unused, rx) = mpsc::channel::<Frame>(1);
        let conn = NdjsonConn {
            tx,
            rx,
            _permit: Some(permit),
        };
        assert_eq!(p.available_permits(), 1);
        // This is what the streaming path relies on: it never calls `release`,
        // so the permit must come back purely from the drop.
        drop(conn);
        assert_eq!(p.available_permits(), 2);
    }
}
