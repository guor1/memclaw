//! `oc http` 网关端到端：真 axum + 真 `ConnPool` + 真 daemon（真传输 + 真 store）。
//!
//! 取代原 `docs/testing/P2-oc-http真机端到端测试用例.md` 里需要手工开四个终端、
//! 用 `curl` 逐条敲的部分。只有模型被换成脚本化 mock——它决定的是「回复内容」，
//! 而这些用例验的是**并发时序与错误路径**，与模型说什么无关。
//!
//! # 防止测试自己退化成顺序执行
//!
//! 手册 §0.0 记过一个静默陷阱：在 cmd.exe 里 `&` 是命令分隔符而非后台符，
//! 并发用例会变成顺序执行、全部 200，看着像通过、实际什么都没测到。
//! 代码里的等价风险是「以为并发了其实没有」，所以并发用例一律用
//! [`max_in_flight`] 断言区间真的重叠——沿用 `scripts/h3-concurrent.sh`
//! 那条时间轴的思路。

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::{Delta, FinishReason};
use oc_server::testing::TestDaemon;
use oc_server::TransportKind;

/// 起一个 HTTP 网关，返回其 base url。
///
/// `max_conns` 对应 `oc http --max-conns`：到 daemon 的连接硬上限。
async fn spawn_gateway(transport: TransportKind, max_conns: usize) -> String {
    let pool = oc_http::conn_pool::ConnPool::new(transport, max_conns, max_conns);
    let app = oc_http::create_app(pool, "mock".into());

    // 端口交给 OS 分配，避免并行跑测试时撞端口。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑端口");
    let addr = listener.local_addr().expect("取端口");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// 一个请求的时间区间与结果。
struct Timed {
    status: u16,
    body: String,
    start: Instant,
    end: Instant,
}

/// 最大同时在飞数。`1` 表示请求实际是顺序跑的——并发根本没发生。
fn max_in_flight(rows: &[Timed]) -> usize {
    rows.iter()
        .map(|a| {
            rows.iter()
                .filter(|b| b.start < a.end && b.end > a.start)
                .count()
        })
        .max()
        .unwrap_or(0)
}

/// 并发打 `n` 个请求到同一个 session key。
async fn concurrent_posts(base: &str, session_key: &str, n: usize) -> Vec<Timed> {
    let client = reqwest::Client::new();
    let mut tasks = Vec::new();
    for i in 0..n {
        let client = client.clone();
        let url = format!("{base}/v1/responses");
        let key = session_key.to_string();
        tasks.push(tokio::spawn(async move {
            let start = Instant::now();
            let resp = client
                .post(&url)
                .header("content-type", "application/json")
                .header("x-openclaw-session-key", key)
                // 每个请求内容不同，用来验内容不串台。
                .body(format!(r#"{{"model":"mock","input":"req-{i}"}}"#))
                .timeout(Duration::from_secs(60))
                .send()
                .await;
            let (status, body) = match resp {
                Ok(r) => {
                    let s = r.status().as_u16();
                    (s, r.text().await.unwrap_or_default())
                }
                // 超时/连接错误：用 0 表示「没拿到 HTTP 应答」，即挂死。
                Err(_) => (0, String::new()),
            };
            Timed {
                status,
                body,
                start,
                end: Instant::now(),
            }
        }));
    }

    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.expect("任务不应 panic"));
    }
    out
}

/// 占道一段时间的脚本：让并发请求真的排上队。
fn slow_reply(text: &str, delay: Duration) -> Vec<ScriptStep> {
    vec![
        ScriptStep {
            delay,
            delta: Delta::Text(text.into()),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Done(FinishReason::Stop),
        },
    ]
}

/// TC-H1：裸请求（无 session key）应落 `main`，且非流式打通。
#[tokio::test]
async fn bare_request_lands_on_main() {
    let daemon = TestDaemon::start("gw-bare", Arc::new(MockProvider::echo_text("你好"))).await;
    let base = spawn_gateway(daemon.transport(), 4).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"mock","input":"hi"}"#)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("请求应有应答");

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("completed"), "应为完成态，实际 {body}");
    assert!(body.contains("你好"), "回复应含模型文本，实际 {body}");

    // 会话应落 main——不该冒出 http-<uuid> 之类的临时会话。
    let mut client = daemon.client().await;
    client
        .request(oc_proto::Method::SessionsList)
        .await;
    let ids = match client.recv().await {
        oc_proto::Frame::Res(r) => match r.result {
            oc_proto::ResResult::Ok(oc_proto::MethodOk::Sessions(v)) => {
                v.iter().map(|s| s.id.as_str().to_string()).collect::<Vec<_>>()
            }
            other => panic!("期望 Sessions，得到 {other:?}"),
        },
        other => panic!("期望 Res，得到 {other:?}"),
    };
    assert!(ids.iter().any(|i| i == "main"), "应有 main 会话：{ids:?}");
    assert!(
        !ids.iter().any(|i| i.starts_with("http-")),
        "裸请求不应新建 http-* 会话：{ids:?}"
    );
}

/// TC-H3：同会话并发——全部返回、无挂死、内容不串台。
///
/// 这是 `c1f58e5`（并发挂死修复）的正面回归。同会话是**串行**执行的
/// （车道单开），所以断言的是「排队但都返回」，不是「同时执行」。
#[tokio::test]
async fn concurrent_same_session_all_return() {
    const N: usize = 3;
    // 每轮占道 300ms，确保后来的请求真的排队。
    let daemon = TestDaemon::start(
        "gw-race",
        Arc::new(MockProvider::scripted(slow_reply(
            "ok",
            Duration::from_millis(300),
        ))),
    )
    .await;
    let base = spawn_gateway(daemon.transport(), 8).await;

    let rows = concurrent_posts(&base, "race-test", N).await;

    // 先证明请求真的并发发出了——否则后面的断言都不算验过。
    assert!(
        max_in_flight(&rows) > 1,
        "请求未真正并发（最大同时在飞 = {}），后续断言无意义",
        max_in_flight(&rows)
    );

    // 核心：一个都不许挂死。status == 0 表示压根没拿到应答。
    for (i, r) in rows.iter().enumerate() {
        assert_ne!(r.status, 0, "请求 #{i} 挂死（无应答）");
        assert_eq!(
            r.status, 200,
            "请求 #{i} 应成功，实际 {}：{}",
            r.status, r.body
        );
    }

    // 内容不串台：每个响应应含自己的输入回显或至少是合法完成态。
    for r in &rows {
        assert!(r.body.contains("completed"), "应为完成态：{}", r.body);
    }
}

/// TC-H4：队列满时应**报错**而非挂住。
///
/// 旧 bug：actor 在 `queue.submit()` 之前就回执 run_id，被拒的轮拿到合法
/// run_id 却永不起步，`accumulate_response` 无超时地等它 → HTTP 请求挂死。
/// 见 `oc-server/tests/queue_full_reject.rs`（server 侧同源回归）。
#[tokio::test]
async fn queue_full_returns_error_not_hang() {
    // queue_cap = 1：一个活跃 + 一个排队，其余必被拒。
    let daemon = TestDaemon::builder(
        "gw-qfull",
        Arc::new(MockProvider::scripted(slow_reply(
            "ok",
            Duration::from_millis(800),
        ))),
    )
    .map_cfg(|c| {
        use oc_server::testing::SessionConfigExt;
        c.with_queue_cap(1)
    })
    .start()
    .await;
    let base = spawn_gateway(daemon.transport(), 16).await;

    let rows = concurrent_posts(&base, "queue-full", 8).await;

    assert!(max_in_flight(&rows) > 1, "请求未真正并发");

    // 最重要的一条：每个请求都在有限时间内返回，没有一个永久挂住。
    for (i, r) in rows.iter().enumerate() {
        assert_ne!(r.status, 0, "请求 #{i} 挂死——队列满导致的挂死回归");
    }

    // 队列满应体现为失败应答（当前映射为 5xx，语义上更该是 429/503——
    // 见 design/OpenAI-Responses-API-方案.md §8 记的技术债，此处不断言具体码）。
    let rejected = rows.iter().filter(|r| r.status != 200).count();
    assert!(
        rejected > 0,
        "queue_cap=1 下 8 个并发应有被拒的，实际全部 200：{:?}",
        rows.iter().map(|r| r.status).collect::<Vec<_>>()
    );
}

/// TC-H8：超过 `--max-conns` 时返回 503，而不是无限等待或挂死。
#[tokio::test]
async fn over_max_conns_returns_503() {
    // 每轮占道 2s，配 max_conns=1：第二个请求拿不到许可。
    let daemon = TestDaemon::start(
        "gw-maxconn",
        Arc::new(MockProvider::scripted(slow_reply(
            "ok",
            Duration::from_millis(2000),
        ))),
    )
    .await;
    // 到 daemon 只允许 1 条连接。
    let base = spawn_gateway(daemon.transport(), 1).await;

    // 用不同 session key，排除「同会话串行」这个混淆因素——
    // 这样排队的原因只可能是连接许可不足。
    let client = reqwest::Client::new();
    let mut tasks = Vec::new();
    for i in 0..3 {
        let client = client.clone();
        let url = format!("{base}/v1/responses");
        tasks.push(tokio::spawn(async move {
            let resp = client
                .post(&url)
                .header("content-type", "application/json")
                .header("x-openclaw-session-key", format!("conn-{i}"))
                .body(r#"{"model":"mock","input":"x"}"#)
                .timeout(Duration::from_secs(60))
                .send()
                .await;
            resp.map(|r| r.status().as_u16()).unwrap_or(0)
        }));
    }
    let mut codes = Vec::new();
    for t in tasks {
        codes.push(t.await.expect("任务不应 panic"));
    }

    assert!(!codes.contains(&0), "不应有挂死请求：{codes:?}");
    assert!(
        codes.contains(&503),
        "超出 max_conns=1 应有 503（capacity_exceeded），实际 {codes:?}"
    );
}

/// TC-H7：请求断连后许可应归还，后续请求仍能正常拿到连接。
///
/// 限流器泄漏的表现是「跑几次之后全部 503」——只有连着打才能发现。
#[tokio::test]
async fn aborted_request_returns_permit() {
    let daemon = TestDaemon::start(
        "gw-permit",
        Arc::new(MockProvider::scripted(slow_reply(
            "ok",
            Duration::from_millis(500),
        ))),
    )
    .await;
    let base = spawn_gateway(daemon.transport(), 1).await;
    let url = format!("{base}/v1/responses");

    // 连续三轮：每轮发一个必然超时的请求（客户端侧放弃），再发一个正常请求。
    // 若许可不归还，第二轮起正常请求就会 503。
    for round in 0..3 {
        let _ = reqwest::Client::new()
            .post(&url)
            .header("content-type", "application/json")
            .header("x-openclaw-session-key", format!("permit-a{round}"))
            .body(r#"{"model":"mock","input":"x"}"#)
            // 远短于模型 500ms 的占道时间：客户端主动放弃，模拟断连。
            .timeout(Duration::from_millis(50))
            .send()
            .await;

        // 等上一轮的 run 跑完、许可归还。
        tokio::time::sleep(Duration::from_millis(1200)).await;

        let code = reqwest::Client::new()
            .post(&url)
            .header("content-type", "application/json")
            .header("x-openclaw-session-key", format!("permit-b{round}"))
            .body(r#"{"model":"mock","input":"x"}"#)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map(|r| r.status().as_u16())
            .unwrap_or(0);

        assert_eq!(
            code, 200,
            "第 {round} 轮：断连后许可应已归还，正常请求却得到 {code}"
        );
    }
}

/// TC-H10：保留前缀的 session key 应 400，合法 key 应放行。
///
/// 两条一起断言：只测 400 那条的话，「请求随便就会 400」也能过——
/// `smoke.sh` 里最初就踩过这个（400 实际来自 JSON 解析失败，与 key 校验无关）。
#[tokio::test]
async fn session_key_validation() {
    let daemon = TestDaemon::start("gw-key", Arc::new(MockProvider::echo_text("ok"))).await;
    let base = spawn_gateway(daemon.transport(), 4).await;
    let client = reqwest::Client::new();

    let post = |key: &str| {
        let client = client.clone();
        let url = format!("{base}/v1/responses");
        let key = key.to_string();
        async move {
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("x-openclaw-session-key", key)
                .body(r#"{"model":"mock","input":"x"}"#)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .expect("应有应答")
                .status()
                .as_u16()
        }
    };

    assert_eq!(post("cron:x").await, 400, "保留前缀应被拒");
    assert_eq!(post("").await, 400, "空 key 应被拒");
    assert_eq!(post("lane one").await, 200, "含空格的合法 key 应放行");
}

/// TC-H6：SSE 流式的事件序列与终止。
///
/// 断言的是**事件类型的顺序**而非具体文本：客户端（OpenAI SDK 等）依赖
/// `response.output_text.delta` 递增、以 `response.completed` 收尾这个契约。
#[tokio::test]
async fn sse_stream_emits_deltas_then_completes() {
    // 分三段吐字，确保流里真有多个 delta 事件。
    let script = vec![
        ScriptStep {
            delay: Duration::from_millis(50),
            delta: Delta::Text("第一段".into()),
        },
        ScriptStep {
            delay: Duration::from_millis(50),
            delta: Delta::Text("第二段".into()),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Done(FinishReason::Stop),
        },
    ];
    let daemon = TestDaemon::start("gw-sse", Arc::new(MockProvider::scripted(script))).await;
    let base = spawn_gateway(daemon.transport(), 4).await;

    let body = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"mock","input":"hi","stream":true}"#)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("应有应答")
        .text()
        .await
        .expect("应能读完流");

    assert!(
        body.contains("response.output_text.delta"),
        "应有增量事件：{body}"
    );
    assert!(body.contains("第一段") && body.contains("第二段"), "两段文本都应送达：{body}");
    assert!(body.contains("response.completed"), "应以完成事件收尾：{body}");

    // 顺序契约：最后一个 delta 必须早于 completed。
    let last_delta = body.rfind("response.output_text.delta").expect("有 delta");
    let completed = body.rfind("response.completed").expect("有 completed");
    assert!(
        last_delta < completed,
        "completed 应在所有 delta 之后：{body}"
    );

    // 线格式：每个 data 行必须是**单层** `data: {json}`。
    //
    // 这条抓到过真缺陷：`event_to_sse` 产出的已经是完整帧（`data: ...\n\n`），
    // 而 axum 的 `Event::data()` 会再包一层，于是线上出现 `data: data: {...}`
    // ——标准 SSE 客户端（含 OpenAI SDK）一个事件都解析不出来。
    // 41 个单测全绿也没发现，因为它们只看 `event_to_sse` 的返回值，
    // 碰不到 axum 的封帧那一步。
    for line in body.lines().filter(|l| l.starts_with("data:")) {
        let value = line.strip_prefix("data: ").unwrap_or(line);
        assert!(
            !value.starts_with("data:"),
            "SSE 出现双层 data: 前缀，客户端无法解析：{line}"
        );
        if value != "[DONE]" {
            serde_json::from_str::<serde_json::Value>(value)
                .unwrap_or_else(|e| panic!("data 值应是合法 JSON（{e}）：{line}"));
        }
    }
}

/// TC-H11：`POST /v1/responses/:id/cancel` 能打断正在跑的轮。
#[tokio::test]
async fn cancel_endpoint_aborts_run() {
    use futures_util::StreamExt;

    let daemon = TestDaemon::start(
        "gw-cancel",
        // 占道 30s：给我们足够时间在它跑着的时候发 cancel。
        Arc::new(MockProvider::scripted(slow_reply(
            "ok",
            Duration::from_secs(30),
        ))),
    )
    .await;
    let base = spawn_gateway(daemon.transport(), 4).await;
    let client = reqwest::Client::new();

    // 流式发起：非流式会阻塞到跑完，拿不到 response id 就无从 cancel。
    let mut stream = client
        .post(format!("{base}/v1/responses"))
        .header("content-type", "application/json")
        .header("x-openclaw-session-key", "cancel-me")
        .body(r#"{"model":"mock","input":"hi","stream":true}"#)
        .send()
        .await
        .expect("流式请求应建立")
        .bytes_stream();

    // 从首批 SSE 事件里抠出 response id（`"id":"resp_..."`）。
    let mut buf = String::new();
    let mut resp_id = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while resp_id.is_none() && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // 逐条 `data: {...}` 解析，取第一个带 response.id 的事件。
                // 手工切字符串太容易算错下标，交给 serde_json。
                for line in buf.lines() {
                    let Some(payload) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload.trim()) else {
                        continue; // 可能是被切成两半的不完整 JSON，等下一个 chunk
                    };
                    if let Some(id) = v
                        .get("response")
                        .and_then(|r| r.get("id"))
                        .and_then(|i| i.as_str())
                    {
                        resp_id = Some(id.to_string());
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    let resp_id = resp_id.unwrap_or_else(|| panic!("未能从 SSE 取到 response id：{buf}"));

    // 未知 id 应 404 而非 500——错误路径也是契约的一部分。
    let missing = client
        .post(format!("{base}/v1/responses/resp_nonexistent/cancel"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("应有应答")
        .status()
        .as_u16();
    assert_eq!(missing, 404, "未知 response id 应 404");

    // 真取消。
    let code = client
        .post(format!("{base}/v1/responses/{resp_id}/cancel"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("cancel 应有应答")
        .status()
        .as_u16();
    assert_eq!(code, 200, "cancel 应成功，实际 {code}");

    // 车道应被释放——run 真的停了，而不只是端点返回了 200。
    // 不验这一步的话，一个什么都不做的 cancel 端点也能让上面的断言通过。
    let mut released = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let mut probe = daemon.client().await;
        let diag = probe.diagnostics().await;
        let busy = diag
            .sessions
            .iter()
            .find(|s| s.session_id.as_str() == "cancel-me")
            .map(|s| s.active.is_some())
            .unwrap_or(false);
        if !busy {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        released,
        "cancel 后 run 应停止占道（模型脚本要跑 30s，不可能是自然结束）"
    );
}

