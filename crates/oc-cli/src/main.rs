//! oc CLI 入口（设计 §9）。
//!
//! M1 落地 `doctor`：建库/迁移检查 + schema 导出 + 配置校验。
//! 其余子命令（serve/onboard/agent/...）随里程碑加入，当前为占位。

mod doctor;
mod paths;

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
        /// 导出协议 JSON Schema 到 schema/oc-proto.json。
        #[arg(long)]
        dump_schema: bool,
    },
    /// 启动常驻进程（M2+）。
    Serve,
    /// 交互式初始化（M6）。
    Onboard,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Doctor { dump_schema }) => doctor::run(dump_schema),
        Some(Command::Serve) => {
            eprintln!("oc serve: 常驻进程将在 M2 实现");
            Ok(())
        }
        Some(Command::Onboard) => {
            eprintln!("oc onboard: 将在 M6 实现");
            Ok(())
        }
        // 无子命令 → 未来连 daemon 进 TUI；当前提示。
        None => {
            eprintln!("oc: TUI 将在 M2 实现。当前可用：oc doctor");
            Ok(())
        }
    }
}
