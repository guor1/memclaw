# 构建脚本说明

## Ubuntu 部署脚本

本目录提供两种 Ubuntu 构建方式：

### 方式 1：在 Ubuntu 环境直接构建（推荐）

**适用场景**：在 Ubuntu 服务器或 WSL 环境里直接构建

```bash
# 在 Ubuntu 环境执行
cd /path/to/memclaw
./scripts/build-ubuntu.sh
```

**输出**：
- `dist/oc-ubuntu-YYYYMMDD-HHMMSS/` — 包含二进制 + 配置文件 + 安装脚本
- `dist/oc-ubuntu-YYYYMMDD-HHMMSS.tar.gz` — 压缩包

**依赖**（脚本会自动检查并提示安装）：
- Rust 1.90+ (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- libsqlite3-dev (`sudo apt-get install libsqlite3-dev pkg-config`)

---

### 方式 2：在 Windows/macOS 用 Docker 交叉编译

**适用场景**：本地是 Windows/macOS，需要构建 Linux 二进制

```bash
# 在 Windows Git Bash 或 macOS Terminal 执行
cd /path/to/memclaw
./scripts/build-ubuntu-in-docker.sh
```

**依赖**：
- Docker Desktop（会在容器里编译）

**输出**：
- `dist-docker/oc-ubuntu-YYYYMMDD-HHMMSS.tar.gz`

---

## 部署到 Ubuntu 服务器

### 1. 上传压缩包

```bash
scp dist/oc-ubuntu-*.tar.gz user@server:/tmp/
```

### 2. 解压并安装

```bash
ssh user@server
cd /tmp
tar -xzf oc-ubuntu-*.tar.gz
cd oc-ubuntu-*
./install.sh
```

**`install.sh` 会自动**：
- 安装 SQLite 运行时库（libsqlite3-0）
- 复制二进制到 `/usr/local/bin/oc`
- 创建 `~/.oc/config.toml`（自动设置 `transport = "unix"`）

### 3. 配置 API Key

```bash
# 方式 1：环境变量（推荐）
echo 'export DEEPSEEK_API_KEY=sk-xxxx' >> ~/.bashrc
source ~/.bashrc

# 方式 2：直接改配置文件
nano ~/.oc/config.toml
# 修改 api_key 行
```

### 4. 验证并启动

```bash
oc doctor           # 验证配置
oc daemon start     # 启动守护进程
oc tui              # 交互式 TUI
```

---

## 生产环境配置建议

编辑 `~/.oc/config.toml`：

```toml
[server]
transport = "unix"  # Linux 必须用 unix

[memory]
vec = false         # M5 未完成前关闭向量化

[tools.approval]
mode = "prompt"     # 生产环境建议每次询问

[watchdog]
run_timeout_secs = 3600  # 设置 1 小时超时（防卡死）
```

---

## 故障排查

### 问题 1：`oc: command not found`

```bash
# 检查是否在 PATH 里
which oc
# 手动添加（如果安装脚本失败）
echo 'export PATH=/usr/local/bin:$PATH' >> ~/.bashrc
source ~/.bashrc
```

### 问题 2：`cannot open shared object file: libsqlite3.so.0`

```bash
# 安装运行时库
sudo apt-get update
sudo apt-get install libsqlite3-0
```

### 问题 3：`daemon start` 失败

```bash
# 查看日志
oc doctor          # 诊断配置
journalctl -xe     # 系统日志
```

### 问题 4：API Key 环境变量不生效

```bash
# 检查环境变量
echo $DEEPSEEK_API_KEY

# 如果为空，重新设置
export DEEPSEEK_API_KEY=sk-xxxx
# 或改用配置文件的 api_key = { inline = "sk-xxxx" }
```

---

## 卸载

```bash
sudo rm /usr/local/bin/oc
rm -rf ~/.oc
```

---

## 构建参数说明

Cargo.toml 的 `[profile.release]` 配置：

```toml
opt-level = "s"      # 优化体积（而非速度）
lto = true           # 链接时优化（减小 30% 体积）
codegen-units = 1    # 单个代码生成单元（更好的优化）
strip = true         # 移除调试符号
panic = "unwind"     # 保留 panic 堆栈（设计 §10.3 需要）
```

典型二进制大小：15-25 MB（取决于功能开关）。

---

## 高级：systemd 服务（可选）

生产环境可用 systemd 管理守护进程：

```bash
sudo tee /etc/systemd/system/oc-daemon.service << 'EOF'
[Unit]
Description=oc daemon
After=network.target

[Service]
Type=simple
User=youruser
Environment="DEEPSEEK_API_KEY=sk-xxxx"
ExecStart=/usr/local/bin/oc daemon start
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable oc-daemon
sudo systemctl start oc-daemon
sudo systemctl status oc-daemon
```

**注意**：记得替换 `youruser` 和 `DEEPSEEK_API_KEY`。
