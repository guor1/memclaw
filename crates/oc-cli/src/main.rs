//! oc CLI 入口（设计 §9）。
//!
//! - `oc doctor`：建库/迁移/校验/dump-schema（M1）
//! - `oc serve`：启动常驻进程（M2）
//! - `oc`（无子命令）：连 daemon 进 TUI（M2）

mod cli_client;
mod config_loader;
mod doctor;
mod lock;
mod onboard;
mod paths;
mod provider_setup;
mod skills_loader;
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
    /// 交互式初始化：生成 ~/.oc 骨架（config.toml + SOUL.md 等）。
    Onboard,
    /// 定时任务管理（主动性）。
    #[command(subcommand)]
    Cron(CronCmd),
    /// 记忆检索（调试/自省）。
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// 查看 daemon 状态快照。
    Status,
    /// 列出所有会话。
    Sessions,
    /// 压缩当前会话上下文（摘要旧历史）。
    Compact,
}

#[derive(Subcommand)]
enum CronCmd {
    /// 添加定时任务。EXPR 为 5 字段 cron（分 时 日 月 周）。
    Add {
        /// cron 表达式，如 "0 9 * * 1-5"（工作日 9 点）。
        expr: String,
        /// 触发时执行的提示词。
        prompt: String,
        /// 时区（当前按 UTC 语义处理）。
        #[arg(long, default_value = "UTC")]
        tz: String,
    },
    /// 列出所有定时任务。
    List,
    /// 删除一个定时任务。
    Rm {
        /// 任务 id（见 list）。
        cron_id: String,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// 按词法检索记忆。
    Search {
        /// 查询词。
        query: String,
        /// 返回条数上限。
        #[arg(long)]
        limit: Option<u32>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Some(Command::Doctor { dump_schema }) => doctor::run(dump_schema),
        Some(Command::Serve) => run_serve(),
        Some(Command::Onboard) => onboard::run(),
        Some(Command::Cron(CronCmd::Add { expr, prompt, tz })) => {
            cli_client::cron_add(expr, prompt, tz)
        }
        Some(Command::Cron(CronCmd::List)) => cli_client::cron_list(),
        Some(Command::Cron(CronCmd::Rm { cron_id })) => cli_client::cron_rm(cron_id),
        Some(Command::Memory { cmd: MemoryCmd::Search { query, limit } }) => {
            cli_client::memory_search(query, limit)
        }
        Some(Command::Status) => cli_client::status(),
        Some(Command::Sessions) => cli_client::sessions(),
        Some(Command::Compact) => cli_client::compact(),
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

    let cfg = config_loader::load()?;
    let (provider, session_cfg, heartbeat) = provider_setup::build(&cfg)?;

    let db = paths::db_path()?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let store = oc_store::Store::open_path(db)
            .map_err(|e| anyhow::anyhow!("打开数据库失败: {e}"))?;
        oc_server::serve_with(kind, provider, session_cfg, heartbeat, store)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    })?;
    Ok(())
}
