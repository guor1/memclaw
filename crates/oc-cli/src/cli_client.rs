//! 薄命令行客户端（设计 §9）：连 daemon、发一个请求、收对应应答、返回。
//!
//! 用于 `oc cron`/`oc memory`/`oc status` 等一次性命令（非交互 TUI）。
//! 握手（connect → hello）后发目标方法，按 req id 匹配应答，忽略中途事件帧。

use anyhow::{anyhow, bail, Result};
use oc_proto::{
    ConnectParams, Frame, Method, MethodOk, Req, ReqId, ResResult, PROTO_VERSION,
};
use oc_tui::client::{ClientTransport, ConnectTo};

use crate::paths;

/// 连接 daemon 并完成握手，返回就绪的传输。
async fn connect() -> Result<ClientTransport> {
    let home = paths::oc_home()?;
    let to = ConnectTo::platform_default(&home);
    let mut client = ClientTransport::connect(&to).await.map_err(|e| {
        anyhow!("无法连接 daemon：{e}\n请先在另一个终端运行：oc serve")
    })?;

    // 握手。
    client
        .send(&Frame::Req(Req {
            id: ReqId::new("connect-0"),
            method: Method::Connect(ConnectParams { proto_version: PROTO_VERSION, token: None }),
            idempotency_key: None,
        }))
        .await?;
    // 等 hello（跳过可能先到的事件帧）。
    loop {
        match client.recv().await? {
            Some(Frame::Res(res)) => match res.result {
                ResResult::Ok(_) => break,
                ResResult::Err(e) => bail!("连接被拒: {}", e.message),
            },
            Some(_) => continue,
            None => bail!("daemon 在握手期间断开"),
        }
    }
    Ok(client)
}

/// 发一个方法并等其应答（按 req id 匹配，忽略事件帧）。
async fn call(client: &mut ClientTransport, method: Method) -> Result<MethodOk> {
    let req_id = "cli-1";
    client
        .send(&Frame::Req(Req {
            id: ReqId::new(req_id),
            method,
            idempotency_key: None,
        }))
        .await?;
    loop {
        match client.recv().await? {
            Some(Frame::Res(res)) if res.id.as_str() == req_id => match res.result {
                ResResult::Ok(ok) => return Ok(ok),
                ResResult::Err(e) => bail!("{}", e.message),
            },
            Some(_) => continue, // 事件帧 / 其它应答，跳过
            None => bail!("daemon 断开"),
        }
    }
}

/// 在一个临时 runtime 上跑一次 request/response。
fn run_once<F>(f: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(f)
}

// ── 命令实现 ────────────────────────────────────────────────────

pub fn cron_add(expr: String, prompt: String, tz: String) -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(
            &mut c,
            Method::CronAdd(oc_proto::CronAddParams { expr, prompt, tz }),
        )
        .await?;
        if let MethodOk::CronAdd { cron_id } = ok {
            println!("已添加定时任务：{}", cron_id.as_str());
        }
        Ok(())
    })
}

pub fn cron_list() -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(&mut c, Method::CronList).await?;
        if let MethodOk::CronList(list) = ok {
            if list.is_empty() {
                println!("（无定时任务）");
            } else {
                for c in list {
                    let next = c
                        .next_at
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "-".into());
                    let en = if c.enabled { "启用" } else { "停用" };
                    println!(
                        "{}  [{}]  {}  下次:{}  «{}»",
                        c.id.as_str(),
                        en,
                        c.expr,
                        next,
                        c.prompt
                    );
                }
            }
        }
        Ok(())
    })
}

pub fn cron_rm(cron_id: String) -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        call(
            &mut c,
            Method::CronRm(oc_proto::CronRmParams { cron_id: oc_proto::CronId::new(cron_id) }),
        )
        .await?;
        println!("已删除。");
        Ok(())
    })
}

pub fn memory_search(query: String, limit: Option<u32>) -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(
            &mut c,
            Method::MemorySearch(oc_proto::MemSearchParams { query, limit }),
        )
        .await?;
        if let MethodOk::MemorySearch(hits) = ok {
            if hits.is_empty() {
                println!("（无匹配记忆）");
            } else {
                for h in hits {
                    println!("[{:.3}] ({}) {}", h.score, h.tier, h.text);
                }
            }
        }
        Ok(())
    })
}

pub fn status() -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(&mut c, Method::Status).await?;
        if let MethodOk::Status(s) = ok {
            println!(
                "会话:{}  活跃run:{}  排队:{}  后台任务:{}",
                s.session.as_str(),
                s.active_run.map(|r| r.as_str().to_string()).unwrap_or_else(|| "-".into()),
                s.queued_turns,
                s.background_tasks
            );
        }
        Ok(())
    })
}
