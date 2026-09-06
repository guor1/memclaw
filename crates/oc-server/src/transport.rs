//! 本地传输抽象（设计 §2.5）。
//!
//! **AF_UNIX 与 Windows 命名管道是两套 API**，这里用 [`TransportKind`] /
//! [`Listener`] / [`Conn`] 把差异封在一处，上层（[`crate::conn`]）只看到
//! `AsyncRead + AsyncWrite`。

use std::path::PathBuf;

use crate::error::{ServerError, ServerResult};

/// 传输种类（M2：本地 unix socket / 命名管道；WS 为 Phase 2 feature）。
#[derive(Debug, Clone)]
pub enum TransportKind {
    /// Unix domain socket，含 socket 文件路径。
    Unix(PathBuf),
    /// Windows 命名管道，含管道名（如 `\\.\pipe\oc-...`）。
    Pipe(String),
}

/// 端点覆盖环境变量。
///
/// 存在理由：Windows 的默认管道名是**全局常量**（见 [`default_pipe_name`]），
/// `platform_default` 不看 `oc_home`，所以单靠 `OC_HOME` 无法把测试 daemon 与
/// 开发机上正在跑的那个隔开——改了 `OC_HOME` 再跑 `oc sessions`，返回的仍是
/// 原 daemon 的数据。给每个 CLI 子命令加 `--socket` 太啰嗦，故用环境变量统一覆盖：
/// `oc serve` 与所有 CLI/TUI 客户端都尊重它，端到端测试因此能自我隔离。
///
/// 默认不设置，行为与之前完全一致。
pub const SOCKET_ENV: &str = "OC_SOCKET";

/// 读 [`SOCKET_ENV`]（空值视为未设置）。
pub fn socket_override() -> Option<String> {
    std::env::var(SOCKET_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl TransportKind {
    /// 平台默认传输。`OC_SOCKET` 若已设置则优先。
    pub fn platform_default(oc_home: &std::path::Path) -> Self {
        if let Some(s) = socket_override() {
            return Self::from_endpoint(s);
        }
        #[cfg(windows)]
        {
            let _ = oc_home;
            TransportKind::Pipe(default_pipe_name())
        }
        #[cfg(not(windows))]
        {
            TransportKind::Unix(oc_home.join("run").join("oc.sock"))
        }
    }

    /// 按平台语义解释一个显式端点（Windows 管道名 / Unix socket 路径）。
    pub fn from_endpoint(endpoint: impl Into<String>) -> Self {
        let s = endpoint.into();
        #[cfg(windows)]
        {
            TransportKind::Pipe(s)
        }
        #[cfg(not(windows))]
        {
            TransportKind::Unix(std::path::PathBuf::from(s))
        }
    }
}

/// 默认命名管道名（Windows）。
#[cfg(windows)]
pub fn default_pipe_name() -> String {
    // 单用户机器上，进程名即足够隔离；如需多用户可拼接 user SID。
    r"\\.\pipe\oc-daemon".to_string()
}

/// 监听器。
pub enum Listener {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    Pipe {
        name: String,
        server: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    },
}

impl Listener {
    /// 绑定给定传输。
    pub fn bind(kind: &TransportKind) -> ServerResult<Self> {
        match kind {
            #[cfg(unix)]
            TransportKind::Unix(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // 清理陈旧 socket 文件（上次崩溃残留）。
                let _ = std::fs::remove_file(path);
                let listener = tokio::net::UnixListener::bind(path)?;
                Ok(Listener::Unix(listener))
            }
            #[cfg(windows)]
            TransportKind::Pipe(name) => {
                use tokio::net::windows::named_pipe::ServerOptions;
                let server = ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(name)?;
                Ok(Listener::Pipe {
                    name: name.clone(),
                    server: Some(server),
                })
            }
            // 跨平台占位：请求了本平台不支持的传输。
            #[cfg(not(unix))]
            TransportKind::Unix(_) => {
                Err(ServerError::UnsupportedTransport("unix socket".into()))
            }
            #[cfg(not(windows))]
            TransportKind::Pipe(_) => {
                Err(ServerError::UnsupportedTransport("named pipe".into()))
            }
        }
    }

    /// 接受一个连接。
    pub async fn accept(&mut self) -> ServerResult<Conn> {
        match self {
            #[cfg(unix)]
            Listener::Unix(l) => {
                let (stream, _addr) = l.accept().await?;
                Ok(Conn::Unix(stream))
            }
            #[cfg(windows)]
            Listener::Pipe { name, server } => {
                use tokio::net::windows::named_pipe::ServerOptions;
                // 取出当前实例等待客户端连接。
                let this = server
                    .take()
                    .expect("pipe server instance always present between accepts");
                this.connect().await?;
                // 立即为下一个客户端准备新实例（命名管道的标准作法）。
                let next = ServerOptions::new().create(name.as_str())?;
                *server = Some(next);
                Ok(Conn::Pipe(this))
            }
        }
    }
}

/// 单个连接。两种底层各实现 `AsyncRead + AsyncWrite`。
pub enum Conn {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    Pipe(tokio::net::windows::named_pipe::NamedPipeServer),
}

// 手动转发 AsyncRead / AsyncWrite 到内部类型。
impl tokio::io::AsyncRead for Conn {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Conn::Unix(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            Conn::Pipe(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for Conn {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            #[cfg(unix)]
            Conn::Unix(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            Conn::Pipe(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Conn::Unix(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            Conn::Pipe(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Conn::Unix(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            Conn::Pipe(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}
