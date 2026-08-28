//! oc CLI 入口（设计 §9）。
//!
//! - `oc doctor`：建库/迁移/校验/dump-schema（M1）
//! - `oc serve`：启动常驻进程（M2）
//! - `oc`（无子命令）：连 daemon 进 TUI（M2）

mod doctor;
mod lock;
mod paths;
mod tui_runner;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "oc", version, about = "个人助手 daemon 的 CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 建库/迁移检查、schema 导出、配置校验。
    Doctor {
        #[arg(long)]
        dump_schema: bool,
    },
    /// 启动常驻进程。
    Serve,
    /// 交互式初始化（M6）。
    Onboard,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Some(Command::Doctor { dump_schema }) => doctor::run(dump_schema),
        Some(Command::Serve) => run_serve(),
        Some(Command::Onboard) => {
            eprintln!("oc onboard: 将在 M6 实现");
            Ok(())
        }
        // 无子命令 → 连 daemon 进 TUI。
        None => tui_runner::run(),
    }
}

/// 启动常驻进程（阻塞直到收到关停信号）。
fn run_serve() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "oc_server=info".into()),
        )
        .init();

    let home = paths::oc_home()?;
    std::fs::create_dir_all(&home)?;

    // 单实例锁：防止多个 serve 争用同一 socket / 库。
    let _guard = lock::acquire(&home)?;

    let kind = oc_server::TransportKind::platform_default(&home);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move { oc_server::serve(kind).await })?;
    Ok(())
}
