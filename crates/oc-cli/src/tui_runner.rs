//! TUI 启动器：终端 raw mode 进/出 + 运行 [`oc_tui::app::App`]。

use anyhow::{Context, Result};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use oc_tui::app::App;
use oc_tui::client::ConnectTo;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::paths;

pub fn run() -> Result<()> {
    let home = paths::oc_home()?;
    let to = ConnectTo::platform_default(&home);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        // 先尝试连接，失败则给出友好提示（daemon 未启动）。
        let mut app = match App::connect(&to).await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("无法连接 daemon：{e}");
                eprintln!("请先在另一个终端运行：oc serve");
                return Ok(());
            }
        };

        // 进入 TUI 模式。
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut term = Terminal::new(backend)?;

        let result = app.run(&mut term).await;

        // 恢复终端（无论成功失败）。
        disable_raw_mode().ok();
        execute!(term.backend_mut(), LeaveAlternateScreen).ok();
        term.show_cursor().ok();

        result
    })
}
