@echo off
chcp 65001 >nul
REM Connect to daemon and enter TUI (run serve.bat first).
REM Double-click to run. Ctrl-C exits TUI (daemon keeps running).
cd /d "%~dp0"
title oc TUI
cargo run -p oc-cli
echo.
echo [TUI exited] Press any key to close...
pause >nul
