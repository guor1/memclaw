#!/bin/bash
# Ubuntu 构建脚本：在 Ubuntu 环境编译 oc 二进制 + 打包配置文件
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="$PROJECT_ROOT/dist"
PACKAGE_NAME="oc-ubuntu-$(date +%Y%m%d-%H%M%S)"
PACKAGE_DIR="$OUTPUT_DIR/$PACKAGE_NAME"

echo "=== oc Ubuntu 构建脚本 ==="
echo "项目根目录: $PROJECT_ROOT"
echo "输出目录: $OUTPUT_DIR"

# 1. 检查依赖
echo ""
echo "[1/5] 检查依赖..."
if ! command -v cargo &> /dev/null; then
    echo "错误：未找到 cargo，请先安装 Rust (https://rustup.rs/)"
    exit 1
fi
if ! command -v rustc &> /dev/null; then
    echo "错误：未找到 rustc"
    exit 1
fi

RUST_VERSION=$(rustc --version | awk '{print $2}')
echo "Rust 版本: $RUST_VERSION"

# SQLite 开发库检查（rusqlite 编译依赖）
if ! dpkg -l | grep -q libsqlite3-dev; then
    echo "警告：未检测到 libsqlite3-dev，尝试安装..."
    sudo apt-get update && sudo apt-get install -y libsqlite3-dev pkg-config
fi

# 2. 清理旧构建
echo ""
echo "[2/5] 清理旧构建..."
cd "$PROJECT_ROOT"
cargo clean
rm -rf "$OUTPUT_DIR"
mkdir -p "$PACKAGE_DIR"

# 3. 编译 release 二进制
echo ""
echo "[3/5] 编译 release 二进制（优化级别 s + LTO + strip）..."
cargo build --release --bin oc
if [ ! -f "target/release/oc" ]; then
    echo "错误：编译失败，未找到 target/release/oc"
    exit 1
fi

BINARY_SIZE=$(du -h target/release/oc | awk '{print $1}')
echo "二进制大小: $BINARY_SIZE"

# 4. 打包
echo ""
echo "[4/5] 打包..."
cp target/release/oc "$PACKAGE_DIR/"
cp config.example.toml "$PACKAGE_DIR/"
cp README.md "$PACKAGE_DIR/" 2>/dev/null || echo "跳过 README.md（不存在）"

# 创建安装说明
cat > "$PACKAGE_DIR/INSTALL.txt" << 'EOF'
# oc Ubuntu 安装说明

## 1. 安装二进制
sudo cp oc /usr/local/bin/
sudo chmod +x /usr/local/bin/oc

## 2. 创建配置文件
mkdir -p ~/.oc
cp config.example.toml ~/.oc/config.toml

## 3. 修改配置（必须）
编辑 ~/.oc/config.toml：
  - 修改 `transport = "unix"`（Linux 使用 Unix socket）
  - 设置 API Key 环境变量：export DEEPSEEK_API_KEY=sk-xxxx
  - 或直接修改 config.toml 的 api_key 行

## 4. 验证安装
oc doctor

## 5. 命令一览（注意：没有 `oc daemon start`，也没有 `oc tui`）
oc serve                 # 启动常驻进程（前台阻塞，Ctrl-C 停止）
oc                       # 不带子命令 = 连上 daemon 进 TUI 对话
oc http --port 8080      # OpenAI Responses API 兼容网关（仅监听 127.0.0.1）
oc status / oc sessions / oc debug --watch
oc cron add / list / rm

## 6. 无人值守注意事项（cron / oc http）
这些场景没有 TUI 响应审批弹窗。确认 ~/.oc/config.toml 里
[tools.approval] 的 timeout_secs 非 0（默认 120），否则该轮会一直
占着会话车道。

## 环境变量
- OC_HOME: 自定义配置根目录（默认 ~/.oc）
- DEEPSEEK_API_KEY: DeepSeek API Key（推荐）
- ANTHROPIC_API_KEY: Claude API Key（如用 Anthropic）
- RUST_LOG: 日志级别（debug/info/warn/error）

## 卸载
sudo rm /usr/local/bin/oc
rm -rf ~/.oc

## 常见问题
Q: 运行 oc 提示 "cannot open shared object file: libsqlite3.so.0"
A: 安装运行时库 `sudo apt-get install libsqlite3-0`

Q: daemon 启动失败
A: 检查 ~/.oc/config.toml 是否正确，运行 `oc doctor` 诊断
EOF

# 创建一键部署脚本
cat > "$PACKAGE_DIR/install.sh" << 'EOF'
#!/bin/bash
# 一键安装脚本
set -e

echo "=== oc 快速安装 ==="

# 检查 SQLite 运行时库
if ! ldconfig -p | grep -q libsqlite3.so; then
    echo "[依赖] 安装 SQLite 运行时库..."
    sudo apt-get update && sudo apt-get install -y libsqlite3-0
fi

# 安装二进制
echo "[安装] 复制二进制到 /usr/local/bin/..."
sudo cp oc /usr/local/bin/
sudo chmod +x /usr/local/bin/oc

# 创建配置目录
echo "[配置] 创建 ~/.oc/ 目录..."
mkdir -p ~/.oc

# 配置文件处理
if [ -f ~/.oc/config.toml ]; then
    echo "[配置] 检测到已有配置文件，跳过覆盖"
    echo "       如需重置，请手动删除 ~/.oc/config.toml 后重新运行"
else
    echo "[配置] 复制示例配置..."
    cp config.example.toml ~/.oc/config.toml

    # 自动改 transport = "unix"（Linux 默认）
    sed -i 's/transport = "pipe"/transport = "unix"/' ~/.oc/config.toml
    echo "       已自动设置 transport = \"unix\""
fi

# 验证安装
echo ""
echo "[验证] 运行 oc doctor..."
if oc doctor; then
    echo ""
    echo "✅ 安装成功！"
    echo ""
    echo "下一步："
    echo "  1. 设置 API Key: export DEEPSEEK_API_KEY=sk-xxxx"
    echo "  2. 启动常驻进程: oc serve"
    echo "  3. 另开终端进 TUI: oc"
    echo ""
    echo "完整说明见 INSTALL.txt"
else
    echo ""
    echo "⚠️  oc doctor 验证失败，请检查配置"
    echo "   详见 INSTALL.txt 故障排查部分"
fi
EOF
chmod +x "$PACKAGE_DIR/install.sh"

# 创建版本信息
cat > "$PACKAGE_DIR/VERSION.txt" << EOF
构建时间: $(date '+%Y-%m-%d %H:%M:%S %Z')
Git 提交: $(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
Git 分支: $(git branch --show-current 2>/dev/null || echo "unknown")
Rust 版本: $RUST_VERSION
二进制大小: $BINARY_SIZE
目标平台: x86_64-unknown-linux-gnu
EOF

# 5. 生成 tar.gz
echo ""
echo "[5/5] 压缩打包..."
cd "$OUTPUT_DIR"
tar -czf "$PACKAGE_NAME.tar.gz" "$PACKAGE_NAME"
TARBALL_SIZE=$(du -h "$PACKAGE_NAME.tar.gz" | awk '{print $1}')

echo ""
echo "=== 构建完成 ==="
echo "输出目录: $OUTPUT_DIR/$PACKAGE_NAME/"
echo "压缩包: $OUTPUT_DIR/$PACKAGE_NAME.tar.gz ($TARBALL_SIZE)"
echo ""
echo "包含文件："
ls -lh "$PACKAGE_DIR"
echo ""
echo "部署到 Ubuntu 服务器："
echo "  1. 上传: scp $PACKAGE_NAME.tar.gz user@server:/tmp/"
echo "  2. 解压: tar -xzf /tmp/$PACKAGE_NAME.tar.gz"
echo "  3. 安装: cd $PACKAGE_NAME && ./install.sh"
