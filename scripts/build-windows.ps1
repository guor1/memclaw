# Windows 构建脚本：编译 oc.exe + 打包配置文件
# PowerShell 脚本，双击运行或在 PowerShell 中执行

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Split-Path -Parent $ScriptDir
$OutputDir = Join-Path $ProjectRoot "dist-windows"
$Timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$PackageName = "oc-windows-$Timestamp"
$PackageDir = Join-Path $OutputDir $PackageName

Write-Host "=== oc Windows 构建脚本 ===" -ForegroundColor Cyan
Write-Host "项目根目录: $ProjectRoot"
Write-Host "输出目录: $OutputDir"

# 1. 检查依赖
Write-Host "`n[1/5] 检查依赖..." -ForegroundColor Yellow
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "错误：未找到 cargo，请先安装 Rust (https://rustup.rs/)" -ForegroundColor Red
    Write-Host "下载地址: https://win.rustup.rs/x86_64" -ForegroundColor Red
    exit 1
}

$RustVersion = (cargo --version)
Write-Host "Rust 版本: $RustVersion" -ForegroundColor Green

# 2. 清理旧构建
Write-Host "`n[2/5] 清理旧构建..." -ForegroundColor Yellow
Set-Location $ProjectRoot
cargo clean
if (Test-Path $OutputDir) {
    Remove-Item $OutputDir -Recurse -Force
}
New-Item -ItemType Directory -Path $PackageDir -Force | Out-Null

# 3. 编译 release 二进制
Write-Host "`n[3/5] 编译 release 二进制（优化级别 s + LTO + strip）..." -ForegroundColor Yellow
cargo build --release --bin oc

$BinaryPath = Join-Path $ProjectRoot "target\release\oc.exe"
if (-not (Test-Path $BinaryPath)) {
    Write-Host "错误：编译失败，未找到 target\release\oc.exe" -ForegroundColor Red
    exit 1
}

$BinarySize = (Get-Item $BinaryPath).Length / 1MB
Write-Host ("二进制大小: {0:N2} MB" -f $BinarySize) -ForegroundColor Green

# 4. 打包
Write-Host "`n[4/5] 打包..." -ForegroundColor Yellow
Copy-Item $BinaryPath $PackageDir
Copy-Item (Join-Path $ProjectRoot "config.example.toml") $PackageDir
if (Test-Path (Join-Path $ProjectRoot "README.md")) {
    Copy-Item (Join-Path $ProjectRoot "README.md") $PackageDir
}

# 创建安装说明
$InstallGuide = @"
# oc Windows 安装说明

## 自动安装（推荐）

双击运行 install.bat，会自动完成以下步骤。

## 手动安装

### 1. 复制二进制到系统路径

方式 A（推荐）：复制到 C:\Windows\System32\
```powershell
# 以管理员身份运行 PowerShell
Copy-Item oc.exe C:\Windows\System32\
```

方式 B：添加到用户目录
```powershell
# 创建用户 bin 目录
`$BinDir = "$env:USERPROFILE\bin"
New-Item -ItemType Directory -Path `$BinDir -Force
Copy-Item oc.exe `$BinDir

# 添加到 PATH（永久生效）
`$OldPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (`$OldPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable("Path", "`$OldPath;`$BinDir", "User")
}
# 重新打开 PowerShell/Terminal 生效
```

### 2. 创建配置文件

```powershell
# 创建配置目录（Windows 默认路径：C:\Users\你\.oc\）
`$OcHome = "$env:USERPROFILE\.oc"
New-Item -ItemType Directory -Path `$OcHome -Force

# 复制配置文件
Copy-Item config.example.toml `$OcHome\config.toml
```

### 3. 修改配置（必须）

编辑 `C:\Users\你\.oc\config.toml`：

```toml
[server]
transport = "pipe"  # Windows 必须用 pipe

[models]
# 设置 API Key（三选一）：
api_key = { env = "DEEPSEEK_API_KEY" }           # 环境变量（推荐）
# api_key = { file = "C:\\Users\\你\\.oc\\deepseek.key" }  # 文件
# api_key = { inline = "sk-xxxx" }               # 直接写（不推荐）
```

### 4. 设置 API Key

方式 A（推荐）：环境变量
```powershell
# 设置用户环境变量（永久生效）
[Environment]::SetEnvironmentVariable("DEEPSEEK_API_KEY", "sk-xxxx", "User")
# 重新打开 PowerShell/Terminal 生效
```

方式 B：配置文件
直接在 `config.toml` 中设置 `api_key = { inline = "sk-xxxx" }`

### 5. 验证安装

```powershell
# 重新打开 PowerShell/Terminal
oc doctor
```

### 6. 启动使用

```powershell
oc daemon start     # 启动守护进程
oc tui              # 交互式 TUI
```

## 卸载

```powershell
# 删除二进制（如果复制到 System32）
Remove-Item C:\Windows\System32\oc.exe

# 删除配置
Remove-Item "$env:USERPROFILE\.oc" -Recurse -Force

# 删除环境变量
[Environment]::SetEnvironmentVariable("DEEPSEEK_API_KEY", `$null, "User")
```

## 故障排查

### 问题 1：oc : 无法将"oc"项识别为 cmdlet、函数、脚本文件或可运行程序的名称

**原因**：oc.exe 不在 PATH 里，或刚添加 PATH 未生效

**解决**：
1. 重新打开 PowerShell/Terminal（新 PATH 生效）
2. 或用完整路径运行：`C:\Windows\System32\oc.exe doctor`
3. 或检查 PATH：`$env:Path -split ';'`

### 问题 2：daemon start 失败

**原因**：配置文件有误或 API Key 未设置

**解决**：
1. 运行 `oc doctor` 诊断
2. 检查 `C:\Users\你\.oc\config.toml` 的 transport 是否为 "pipe"
3. 确认环境变量：`$env:DEEPSEEK_API_KEY`

### 问题 3：API Key 环境变量不生效

**原因**：设置后未重启 PowerShell/Terminal

**解决**：
1. 关闭所有 PowerShell/Terminal 窗口，重新打开
2. 或用临时环境变量：`$env:DEEPSEEK_API_KEY="sk-xxxx"; oc daemon start`
3. 或改用配置文件的 `api_key = { inline = "..." }`

### 问题 4：防火墙/杀毒软件拦截

**原因**：首次运行的 .exe 可能被拦截

**解决**：
1. Windows Defender: 设置 -> 病毒和威胁防护 -> 管理设置 -> 排除项 -> 添加 oc.exe
2. 或右键 oc.exe -> 属性 -> 解除锁定

## 配置建议（生产环境）

编辑 `C:\Users\你\.oc\config.toml`：

```toml
[server]
transport = "pipe"           # Windows 必须 pipe

[memory]
vec = false                  # M5 未完成前关闭

[tools.approval]
mode = "prompt"              # 安全：每次询问

[watchdog]
run_timeout_secs = 3600      # 防卡死：1 小时超时
```

## Windows 服务（可选）

高级用户可用 NSSM 将 oc daemon 注册为 Windows 服务：

1. 下载 NSSM: https://nssm.cc/download
2. 以管理员身份运行：
```cmd
nssm install oc-daemon "C:\Windows\System32\oc.exe" "daemon start"
nssm set oc-daemon AppEnvironmentExtra DEEPSEEK_API_KEY=sk-xxxx
nssm start oc-daemon
```

3. 管理服务：
```cmd
nssm status oc-daemon
nssm stop oc-daemon
nssm remove oc-daemon confirm
```
"@

Set-Content -Path (Join-Path $PackageDir "INSTALL.txt") -Value $InstallGuide -Encoding UTF8

# 创建自动安装脚本
$InstallBat = @"
@echo off
REM oc Windows 自动安装脚本
echo === oc 快速安装 ===
echo.

REM 检查管理员权限
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo [错误] 需要管理员权限，请右键点击本文件，选择"以管理员身份运行"
    pause
    exit /b 1
)

REM 1. 复制二进制
echo [1/4] 复制 oc.exe 到 C:\Windows\System32\...
copy /Y oc.exe C:\Windows\System32\
if %errorLevel% neq 0 (
    echo [错误] 复制失败
    pause
    exit /b 1
)
echo       完成

REM 2. 创建配置目录
echo.
echo [2/4] 创建配置目录 %USERPROFILE%\.oc\...
if not exist "%USERPROFILE%\.oc" mkdir "%USERPROFILE%\.oc"
echo       完成

REM 3. 复制配置文件
echo.
echo [3/4] 配置文件...
if exist "%USERPROFILE%\.oc\config.toml" (
    echo       检测到已有配置文件，跳过覆盖
    echo       如需重置，请手动删除 %USERPROFILE%\.oc\config.toml 后重新运行
) else (
    copy /Y config.example.toml "%USERPROFILE%\.oc\config.toml"
    echo       已复制示例配置
)

REM 4. 验证
echo.
echo [4/4] 验证安装...
oc doctor
if %errorLevel% neq 0 (
    echo.
    echo [警告] oc doctor 验证失败
    echo        可能原因：API Key 未设置
    echo        请按 INSTALL.txt 的说明设置 DEEPSEEK_API_KEY 环境变量
) else (
    echo.
    echo ✅ 安装成功！
    echo.
    echo 下一步：
    echo   1. 设置 API Key（如果尚未设置）：
    echo      打开 PowerShell，运行：
    echo      [Environment]::SetEnvironmentVariable("DEEPSEEK_API_KEY", "sk-xxxx", "User"^)
    echo      然后重新打开 PowerShell/Terminal
    echo   2. 启动守护进程: oc daemon start
    echo   3. 交互式 TUI: oc tui
    echo.
    echo 完整说明见 INSTALL.txt
)

echo.
pause
"@

Set-Content -Path (Join-Path $PackageDir "install.bat") -Value $InstallBat -Encoding ASCII

# 创建版本信息
$GitCommit = if (Get-Command git -ErrorAction SilentlyContinue) {
    (git rev-parse --short HEAD 2>$null)
} else {
    "unknown"
}
$GitBranch = if (Get-Command git -ErrorAction SilentlyContinue) {
    (git branch --show-current 2>$null)
} else {
    "unknown"
}

$VersionInfo = @"
构建时间: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss zzz')
Git 提交: $GitCommit
Git 分支: $GitBranch
Rust 版本: $RustVersion
二进制大小: $("{0:N2} MB" -f $BinarySize)
目标平台: x86_64-pc-windows-msvc
"@

Set-Content -Path (Join-Path $PackageDir "VERSION.txt") -Value $VersionInfo -Encoding UTF8

# 5. 生成 zip 压缩包
Write-Host "`n[5/5] 压缩打包..." -ForegroundColor Yellow
$ZipPath = Join-Path $OutputDir "$PackageName.zip"
Compress-Archive -Path $PackageDir -DestinationPath $ZipPath -Force
$ZipSize = (Get-Item $ZipPath).Length / 1MB

Write-Host "`n=== 构建完成 ===" -ForegroundColor Green
Write-Host "输出目录: $PackageDir"
Write-Host "压缩包: $ZipPath ($("{0:N2} MB" -f $ZipSize))"
Write-Host "`n包含文件："
Get-ChildItem $PackageDir | Format-Table Name, @{Label="Size";Expression={"{0:N2} MB" -f ($_.Length / 1MB)}}

Write-Host "`n使用说明："
Write-Host "1. 解压 $PackageName.zip"
Write-Host "2. 右键点击 install.bat，选择"以管理员身份运行""
Write-Host "3. 按提示设置 API Key 环境变量"
Write-Host "4. 运行 oc doctor 验证"

Write-Host "`n详见 INSTALL.txt" -ForegroundColor Cyan
