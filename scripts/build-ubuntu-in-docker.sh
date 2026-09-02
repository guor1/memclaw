#!/bin/bash
# 在 Windows/macOS 上通过 Docker 交叉编译 Ubuntu 版本
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "=== oc Ubuntu Docker 构建脚本 ==="
echo "项目根目录: $PROJECT_ROOT"

# 检查 Docker
if ! command -v docker &> /dev/null; then
    echo "错误：未找到 docker，请先安装 Docker Desktop"
    exit 1
fi

# 创建临时 Dockerfile
DOCKERFILE="$PROJECT_ROOT/Dockerfile.build"
cat > "$DOCKERFILE" << 'EOF'
FROM rust:1.90-slim-bookworm

# 安装构建依赖
RUN apt-get update && apt-get install -y \
    libsqlite3-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# 复制项目文件
COPY . .

# 构建 release 二进制
RUN cargo build --release --bin oc

# 输出信息
RUN ls -lh target/release/oc && \
    ldd target/release/oc || true
EOF

echo ""
echo "[1/3] 构建 Docker 镜像..."
docker build -f "$DOCKERFILE" -t oc-builder "$PROJECT_ROOT"

echo ""
echo "[2/3] 从容器中提取二进制..."
CONTAINER_ID=$(docker create oc-builder)
mkdir -p "$PROJECT_ROOT/dist-docker"
docker cp "$CONTAINER_ID:/build/target/release/oc" "$PROJECT_ROOT/dist-docker/"
docker rm "$CONTAINER_ID"

echo ""
echo "[3/3] 打包..."
cd "$PROJECT_ROOT/dist-docker"
PACKAGE_NAME="oc-ubuntu-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$PACKAGE_NAME"
mv oc "$PACKAGE_NAME/"
cp "$PROJECT_ROOT/config.example.toml" "$PACKAGE_NAME/"

# 复制安装脚本（从另一个构建脚本生成的，这里简化版）
cat > "$PACKAGE_NAME/install.sh" << 'INSTALL_EOF'
#!/bin/bash
set -e
echo "=== oc 快速安装 ==="
sudo apt-get update && sudo apt-get install -y libsqlite3-0
sudo cp oc /usr/local/bin/
sudo chmod +x /usr/local/bin/oc
mkdir -p ~/.oc
if [ ! -f ~/.oc/config.toml ]; then
    cp config.example.toml ~/.oc/config.toml
    sed -i 's/transport = "pipe"/transport = "unix"/' ~/.oc/config.toml
fi
echo "✅ 安装完成！运行 'oc doctor' 验证"
INSTALL_EOF
chmod +x "$PACKAGE_NAME/install.sh"

cat > "$PACKAGE_NAME/INSTALL.txt" << 'INSTALL_TXT'
# oc Ubuntu 安装说明（Docker 构建）

## 快速安装
./install.sh

## 手动安装
1. sudo cp oc /usr/local/bin/
2. mkdir -p ~/.oc && cp config.example.toml ~/.oc/config.toml
3. 编辑 ~/.oc/config.toml，设置 transport = "unix"
4. export DEEPSEEK_API_KEY=sk-xxxx
5. oc doctor

## 依赖
sudo apt-get install libsqlite3-0
INSTALL_TXT

tar -czf "$PACKAGE_NAME.tar.gz" "$PACKAGE_NAME"
TARBALL_SIZE=$(du -h "$PACKAGE_NAME.tar.gz" | awk '{print $1}')

echo ""
echo "=== 构建完成（Docker） ==="
echo "输出: $PROJECT_ROOT/dist-docker/$PACKAGE_NAME.tar.gz ($TARBALL_SIZE)"
echo ""
echo "部署到 Ubuntu："
echo "  scp dist-docker/$PACKAGE_NAME.tar.gz user@server:/tmp/"
echo "  ssh user@server 'cd /tmp && tar -xzf $PACKAGE_NAME.tar.gz && cd $PACKAGE_NAME && ./install.sh'"

# 清理 Dockerfile
rm -f "$DOCKERFILE"
