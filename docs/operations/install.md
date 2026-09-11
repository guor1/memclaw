# 安装指南

## 支持的平台

| 平台 | 架构 | 传输方式 | 运行时依赖 |
|------|------|----------|-----------|
| Ubuntu / Linux | x86_64 | unix socket | libsqlite3-0 |
| macOS | x86_64 (Intel) | unix socket | 无（SQLite bundled）|
| macOS | arm64 (Apple Silicon) | unix socket | 无 |
| Windows 10/11 | x86_64 | 命名管道 | 无（静态链接）|

## 从 Release 安装

从 [GitHub Releases](https://github.com/guorui1/oh-my-claw/releases) 下载对应平台的压缩包，解压后运行 `install.sh`（Linux/macOS）或按 `INSTALL.txt`（Windows）操作。

## 从源码构建

前置要求：Rust 1.90+

```bash
# Linux/macOS
cargo build --release --bin oc
# 产物：target/release/oc
```

Windows 见 `scripts/build-windows.ps1`。

构建选项（`Cargo.toml [profile.release]`）：`opt-level = "s"`、`lto = true`、`strip = true`，产物约 15–25 MB。

### Linux 额外依赖

Ubuntu/Debian：

```bash
apt-get install libsqlite3-dev
```

macOS 和 Windows 使用 SQLite bundled 构建，不需要系统依赖。

## 官方构建脚本

| 脚本 | 说明 |
|------|------|
| `scripts/build-ubuntu.sh` | Ubuntu 本地直接构建 |
| `scripts/build-ubuntu-in-docker.sh` | 在 Docker 里交叉编译 Ubuntu 二进制（适合 Windows/macOS 开发机）|
| `scripts/build-windows.ps1` | Windows PowerShell 一键构建 |
