# 构建脚本说明

本目录提供多平台构建脚本：**Ubuntu**（服务器部署）、**Windows**（桌面使用）。

---

## Windows 构建脚本（新增）

### 一键构建

```powershell
# 在 PowerShell 中执行
cd C:\dev\workspace\oh-my-claw
.\scripts\build-windows.ps1
```

**输出**：
- `dist-windows\oc-windows-YYYYMMDD-HHMMSS\` — 包含 oc.exe + 配置 + 安装脚本
- `dist-windows\oc-windows-YYYYMMDD-HHMMSS.zip` — 压缩包

**依赖**：
- Rust 1.90+ (下载：https://win.rustup.rs/x86_64)
- Windows 10/11

### 安装使用

#### 方式 1：自动安装（推荐）

1. 解压 `oc-windows-*.zip`
2. **右键点击 `install.bat`，选择"以管理员身份运行"**
3. 脚本会自动：
   - ✅ 复制 oc.exe 到 `C:\Windows\System32\`
   - ✅ 创建 `C:\Users\你\.oc\config.toml`（自动设置 `transport="pipe"`）
   - ✅ 运行 `oc doctor` 验证
4. 设置 API Key（重要）：
   ```powershell
   # 打开新的 PowerShell 窗口
   [Environment]::SetEnvironmentVariable("DEEPSEEK_API_KEY", "sk-xxxx", "User")
   # 关闭并重新打开 PowerShell 生效
   ```
5. 启动：
   ```powershell
   oc serve    # 前台阻塞，Ctrl-C 停止
   oc          # 另开终端：不带子命令 = 进 TUI 对话
   ```

#### 方式 2：手动安装

```powershell
# 1. 复制二进制（需管理员权限）
Copy-Item oc.exe C:\Windows\System32\

# 2. 创建配置
$OcHome = "$env:USERPROFILE\.oc"
New-Item -ItemType Directory -Path $OcHome -Force
Copy-Item config.example.toml $OcHome\config.toml

# 3. 设置 API Key
[Environment]::SetEnvironmentVariable("DEEPSEEK_API_KEY", "sk-xxxx", "User")

# 4. 验证（重新打开 PowerShell）
oc doctor
```

### Windows 特殊说明

**必须配置项**：
```toml
[server]
transport = "pipe"  # Windows 必须用 pipe（不是 unix）
```

**环境变量生效**：
- 设置环境变量后，**必须关闭所有 PowerShell/Terminal 窗口并重新打开**
- 或用临时变量：`$env:DEEPSEEK_API_KEY="sk-xxxx"; oc serve`

**防火墙/杀毒软件**：
- 首次运行可能被 Windows Defender 拦截
- 解决：设置 -> 病毒和威胁防护 -> 排除项 -> 添加 oc.exe

**Windows 服务（可选）**：
- 用 NSSM 注册为系统服务：https://nssm.cc/download
- 详见 `INSTALL.txt`

---

## Ubuntu 部署脚本

本目录提供两种 Ubuntu 构建方式：

### 方式 1：在 Ubuntu 环境直接构建（推荐）

**适用场景**：在 Ubuntu 服务器或 WSL 环境里直接构建

```bash
# 在 Ubuntu 环境执行
cd /path/to/oh-my-claw
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
cd /path/to/oh-my-claw
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
oc doctor              # 验证配置
oc serve               # 启动常驻进程（前台阻塞，Ctrl-C 停止）
oc                     # 另开终端：不带子命令 = 进 TUI 对话
oc http --port 8080    # 可选：OpenAI 兼容网关（仅监听 127.0.0.1，无鉴权）
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

## 高级：systemd 服务（Ubuntu 可选）

生产环境可用 systemd 管理守护进程：

```bash
sudo tee /etc/systemd/system/oc-daemon.service << 'EOF'
[Unit]
Description=oc daemon
After=network.target

[Service]
Type=simple
User=youruser
# WorkingDirectory 与 ~/.oc 共同构成 file 工具的 allowed_roots：指向仓库或
# 家目录会让 agent 能读写那里的一切。建议先 mkdir -p ~/ocdata 用专用空目录。
WorkingDirectory=/home/youruser/ocdata
Environment="DEEPSEEK_API_KEY=sk-xxxx"
ExecStart=/usr/local/bin/oc serve
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

---

## 平台对比

| 特性 | Windows | Ubuntu |
|------|---------|---------|
| **构建脚本** | `build-windows.ps1` | `build-ubuntu.sh` |
| **二进制名** | `oc.exe` | `oc` |
| **配置目录** | `C:\Users\你\.oc\` | `~/.oc/` |
| **transport** | `pipe` | `unix` |
| **安装方式** | install.bat（管理员） | install.sh |
| **环境变量** | 需重启 PowerShell 生效 | 立即生效（source） |
| **依赖** | 无（静态链接） | libsqlite3-0 |
| **系统服务** | NSSM | systemd |

---

## 快速参考

### Windows
```powershell
# 构建
.\scripts\build-windows.ps1

# 安装（右键 install.bat "以管理员身份运行"）
[Environment]::SetEnvironmentVariable("DEEPSEEK_API_KEY", "sk-xxxx", "User")
oc doctor
oc serve
```

### Ubuntu
```bash
# 构建
./scripts/build-ubuntu.sh

# 安装
./install.sh
export DEEPSEEK_API_KEY=sk-xxxx
oc doctor
oc serve
```
