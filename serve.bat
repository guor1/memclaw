@echo off
chcp 65001 >nul
REM Start oc daemon (foreground, blocking; single-instance lock).
REM Double-click to run; keep window open to see stderr logs. Ctrl-C to stop.
cd /d "%~dp0"
title oc daemon (serve)
set RUST_LOG=oc=debug,oc_server=debug,oc_llm=debug
cargo run -p oc-cli -- serve
echo.
echo [daemon exited] Press any key to close...
pause >nul
