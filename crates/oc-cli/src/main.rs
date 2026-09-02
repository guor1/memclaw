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
mod tz;

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
    /// 话题触发式待办管理（standing intent，主动性）。
    #[command(subcommand)]
    Intent(IntentCmd),
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
    /// 诊断快照：活跃 run、队列深度、车道占用、写线程健康。
    Debug {
        /// 持续刷新（每秒一次），观察卡住时状态如何演变。Ctrl-C 退出。
        #[arg(long)]
        watch: bool,
    },
}

#[derive(Subcommand)]
enum CronCmd {
    /// 添加定时任务。EXPR 为 5 字段 cron（分 时 日 月 周）。
    Add {
        /// cron 表达式，如 "0 9 * * 1-5"（工作日 9 点）。
        expr: String,
        /// 触发时执行的提示词。
        prompt: String,
        /// IANA 时区名，如 Asia/Shanghai。省略则用本机时区。
        ///
        /// 表达式里的时/分按此时区的**本地时间**解释。
        #[arg(long)]
        tz: Option<String>,
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
enum IntentCmd {
    /// 添加话题触发式待办：聊到 KEYWORDS 中任一关键词时，提醒 TEXT。
    ///
    /// 与 cron 的区别：cron 到点触发，intent 由**话题命中**触发。
    /// 例：oc intent add "带转换插头" 出差 德国
    Add {
        /// 触发时注入的提醒正文，如 "带转换插头"。
        text: String,
        /// 触发关键词，可给多个，命中任一即触发。
        #[arg(required = true, num_args = 1..)]
        keywords: Vec<String>,
        /// 两次提醒的最小间隔（秒）。省略取配置 [proactive] 默认。
        #[arg(long)]
        cooldown_secs: Option<i64>,
        /// 最多提醒几次（用尽即静默）。省略取配置默认。
        #[arg(long)]
        budget: Option<u32>,
        /// 多少天后过期；0 = 不过期。省略取配置默认。
        #[arg(long)]
        expiry_days: Option<u32>,
    },
    /// 列出所有话题待办（含已触发次数）。
    List,
    /// 删除一个话题待办。
    Rm {
        /// 待办 id（见 list）。
        intent_id: String,
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
        Some(Command::Intent(IntentCmd::Add {
            text,
            keywords,
            cooldown_secs,
            budget,
            expiry_days,
        })) => cli_client::intent_add(text, keywords, cooldown_secs, budget, expiry_days),
        Some(Command::Intent(IntentCmd::List)) => cli_client::intent_list(),
        Some(Command::Intent(IntentCmd::Rm { intent_id })) => cli_client::intent_rm(intent_id),
        Some(Command::Memory { cmd: MemoryCmd::Search { query, limit } }) => {
            cli_client::memory_search(query, limit)
        }
        Some(Command::Status) => cli_client::status(),
        Some(Command::Sessions) => cli_client::sessions(),
        Some(Command::Compact) => cli_client::compact(),
        Some(Command::Debug { watch }) => cli_client::debug(watch),
        // 无子命令 → 连 daemon 进 TUI。
        None => tui_runner::run(),
    }
}

/// 启动常驻进程（阻塞直到收到关停信号）。
fn run_serve() -> anyhow::Result<()> {
    let home = paths::oc_home()?;
    std::fs::create_dir_all(&home)?;

    // 日志：同时落盘（~/.oc/logs/oc.log.YYYY-MM-DD，按天滚动）+ stderr。
    // daemon 与 TUI 是两个进程，落盘保证排障时能 tail 到 daemon 侧日志。
    // _guard 必须存活到进程结束，否则 non_blocking writer 会丢日志。
    let log_dir = home.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(&log_dir, "oc.log");
    let (file_nb, _guard) = tracing_appender::non_blocking(file_appender);

    use tracing_subscriber::prelude::*;
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "oc=info,oc_server=info,oc_llm=info".into());
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_nb)
        .with_ansi(false)
        .with_target(true)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    tracing::info!(log_dir = %log_dir.display(), "日志已初始化（落盘 + stderr）");

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
