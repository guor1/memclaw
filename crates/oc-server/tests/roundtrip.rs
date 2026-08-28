//! M2 端到端传输测试：起 server，经真实传输（unix socket / 命名管道）
//! 跑一次 connect + chat.send，验证 hello 应答与 echo 事件流。
//!
//! 双平台各验一次（cfg 分派 client 连接），对应 M2 硬验收。

use oc_proto::{
    ChatSendParams, ConnectParams, Frame, LifecyclePhase, Method, Req, ReqId, ResResult,
    PROTO_VERSION,
};
use oc_server::TransportKind;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 连到 server 的 client 流（按平台）。
async fn connect(kind: &TransportKind) -> Box<dyn ClientStream> {
    match kind {
        #[cfg(unix)]
        TransportKind::Unix(path) => {
            let s = tokio::net::UnixStream::connect(path).await.expect("connect unix");
            Box::new(s)
        }
        #[cfg(windows)]
        TransportKind::Pipe(name) => {
            use tokio::net::windows::named_pipe::ClientOptions;
            // 管道可能尚未就绪，重试几次。
            let mut attempts = 0;
            loop {
                match ClientOptions::new().open(name.as_str()) {
                    Ok(c) => return Box::new(c),
                    Err(_) if attempts < 20 => {
                        attempts += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(e) => panic!("connect pipe: {e}"),
                }
            }
        }
        #[allow(unreachable_patterns)]
        _ => panic!("本平台不支持该传输"),
    }
}

/// 抽象读写（unix stream / 命名管道 client 都实现 AsyncRead+AsyncWrite）。
trait ClientStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ClientStream for T {}

fn test_transport() -> TransportKind {
    #[cfg(windows)]
    {
        TransportKind::Pipe(format!(r"\\.\pipe\oc-test-{}", std::process::id()))
    }
    #[cfg(not(windows))]
    {
        let dir = std::env::temp_dir().join(format!("oc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TransportKind::Unix(dir.join("oc.sock"))
    }
}

#[tokio::test]
async fn connect_and_echo_roundtrip() {
    let kind = test_transport();

    // 起 server（mock provider 回显含"你好"的文本，供断言）。
    let server_kind = kind.clone();
    let server = tokio::spawn(async move {
        use std::sync::Arc;
        use std::time::Duration;
        let provider = Arc::new(oc_llm::mock::MockProvider::echo_text("你好，我在。"));
        let cfg = oc_server::SessionConfig {
            model: "mock".into(),
            system_prompt: None,
            idle_timeout: Duration::from_secs(5),
            run_timeout: None,
            queue_cap: 8,
        };
        let _ = oc_server::serve_with(server_kind, provider, cfg, Duration::from_secs(60)).await;
    });

    // 等 listener 就绪。
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let stream = connect(&kind).await;
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);

    // 1) connect → hello
    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("c0"),
        method: Method::Connect(ConnectParams { proto_version: PROTO_VERSION, token: None }),
        idempotency_key: None,
    })).await;

    let hello = recv(&mut reader).await;
    match hello {
        Frame::Res(res) => {
            assert_eq!(res.id.as_str(), "c0");
            assert!(matches!(res.result, ResResult::Ok(_)), "connect 应成功");
        }
        other => panic!("期望 Res(hello)，得到 {other:?}"),
    }

    // 2) chat.send → run_id 应答 + echo 事件流
    send(&mut w, &Frame::Req(Req {
        id: ReqId::new("m1"),
        method: Method::ChatSend(ChatSendParams { session: None, text: "你好".into() }),
        idempotency_key: Some(oc_proto::IdemKey::new("k1")),
    })).await;

    // 收集若干帧，找 run_id 应答、assistant echo、lifecycle end。
    let mut got_run_id = false;
    let mut got_echo = false;
    let mut got_end = false;

    for _ in 0..6 {
        let f = tokio::time::timeout(std::time::Duration::from_secs(2), recv(&mut reader))
            .await
            .expect("不应超时");
        match f {
            Frame::Res(res) if res.id.as_str() == "m1" => {
                assert!(matches!(res.result, ResResult::Ok(_)));
                got_run_id = true;
            }
            Frame::Event(oc_proto::Event::Assistant { delta, .. }) => {
                assert!(delta.contains("你好"), "echo 应含原文");
                got_echo = true;
            }
            Frame::Event(oc_proto::Event::Lifecycle { phase: LifecyclePhase::End, .. }) => {
                got_end = true;
            }
            _ => {}
        }
        if got_run_id && got_echo && got_end {
            break;
        }
    }

    assert!(got_run_id, "应收到 chat.send 的 run_id 应答");
    assert!(got_echo, "应收到 assistant echo 事件");
    assert!(got_end, "应收到 lifecycle end 事件");

    server.abort();
}

async fn send<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &Frame) {
    let mut s = serde_json::to_string(frame).unwrap();
    s.push('\n');
    w.write_all(s.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
}

async fn recv<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Frame {
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line).await.unwrap();
        assert!(n > 0, "对端关闭");
        let t = line.trim_end();
        if t.is_empty() {
            continue;
        }
        return serde_json::from_str(t).unwrap();
    }
}
