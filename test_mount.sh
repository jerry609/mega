#!/bin/bash

# 测试 Scorpio 挂载功能并尝试 Buck2 编译
# 使用原有的 antares 挂载功能

set -e

# 设置日志目录
LOG_DIR="/tmp/scorpio_test_logs"
mkdir -p "$LOG_DIR"

# 设置 RUST_LOG 环境变量（Scorpio 使用 tracing 日志）
export RUST_LOG="debug,scorpio=debug,libfuse_fs::passthrough::newlogfs=debug"

echo "=========================================="
echo "Scorpio 挂载测试脚本（使用 antares）"
echo "=========================================="
echo "日志目录: $LOG_DIR"
echo "RUST_LOG: $RUST_LOG"
echo ""

# 检查是否以 root 运行（FUSE 挂载需要 root）
if [ "$EUID" -ne 0 ]; then 
    echo "警告: 需要 root 权限来挂载 FUSE 文件系统"
    echo "请使用: sudo $0"
    exit 1
fi

# 设置环境变量
export RUSTUP_HOME=/home/master1/.rustup
export CARGO_HOME=/home/master1/.cargo
export CARGO_TARGET_DIR=/home/master1/mega/target-user
export PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig
export PATH="$CARGO_HOME/bin:$PATH"

# 切换到项目目录
cd /home/master1/mega

# 检查 cargo 是否可用
if ! command -v cargo &> /dev/null; then
    echo "错误: cargo 命令未找到"
    echo "请确保 Rust 已安装，或使用完整路径: $CARGO_HOME/bin/cargo"
    exit 1
fi

# 生成唯一的 job_id
JOB_ID="test_$(date +%s)_$$"
echo "Job ID: $JOB_ID"
echo ""

echo "=========================================="
echo "步骤 1: 编译 mount_test"
echo "=========================================="
cargo build -p scorpio --bin mount_test 2>&1 | tee "$LOG_DIR/build.log"

if [ $? -ne 0 ]; then
    echo "编译失败，请查看日志: $LOG_DIR/build.log"
    exit 1
fi

echo ""
echo "=========================================="
echo "步骤 2: 清理旧挂载（如果有）"
echo "=========================================="

# 查找并清理旧的测试挂载
for old_mount in $(find /tmp -maxdepth 2 -type d -name "antares_test_*" 2>/dev/null); do
    if mountpoint -q "$old_mount/mnt" 2>/dev/null; then
        echo "卸载旧测试挂载: $old_mount/mnt"
        fusermount -uz "$old_mount/mnt" 2>/dev/null || true
    fi
    echo "清理旧目录: $old_mount"
    rm -rf "$old_mount" 2>/dev/null || true
done

echo ""
echo "=========================================="
echo "步骤 3: 启动挂载（后台运行）"
echo "=========================================="

echo "启动挂载进程..."
cargo run -p scorpio --bin mount_test -- \
    --config-path scorpio/scorpio.toml \
    --keep-alive 300 \
    2>&1 | tee "$LOG_DIR/mount.log" &
MOUNT_PID=$!

echo "挂载进程 PID: $MOUNT_PID"
sleep 3

# 查找挂载点
MOUNT_POINT=$(find /tmp -maxdepth 2 -type d -path "*/antares_test_*/mnt" 2>/dev/null | head -1)

if [ -z "$MOUNT_POINT" ]; then
    echo "错误: 未找到挂载点"
    echo "查看挂载日志: $LOG_DIR/mount.log"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

echo "挂载点: $MOUNT_POINT"

# 等待一下让挂载稳定
sleep 2

# 检查挂载点是否可访问
if ! mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    echo "警告: 挂载点可能未正确挂载，但继续测试..."
fi

echo ""
echo "=========================================="
echo "步骤 4: 检查挂载点内容"
echo "=========================================="

echo "挂载点根目录内容:"
ls -la "$MOUNT_POINT" | head -20
echo ""

WORK_DIR="$MOUNT_POINT/third-party/buck-hello"
if [ -d "$WORK_DIR" ]; then
    echo "工作目录: $WORK_DIR"
    echo ""
    echo "目录内容:"
    ls -la "$WORK_DIR"
    echo ""
    
    # 检查关键文件
    if [ -f "$WORK_DIR/BUCK" ]; then
        echo "✓ 找到 BUCK 文件"
        echo "BUCK 文件内容（前 20 行）:"
        head -20 "$WORK_DIR/BUCK" 2>/dev/null || echo "  (无法读取)"
        echo ""
    else
        echo "✗ 未找到 BUCK 文件"
    fi
    
    if [ -f "$WORK_DIR/main.rs" ]; then
        echo "✓ 找到 main.rs 文件"
        echo "main.rs 文件内容（前 20 行）:"
        head -20 "$WORK_DIR/main.rs" 2>/dev/null || echo "  (无法读取)"
        echo ""
    else
        echo "✗ 未找到 main.rs 文件"
    fi
    
    # 尝试读取一个文件来测试 Dicfuse
    echo "测试读取文件（验证 Dicfuse 是否工作）:"
    if [ -f "$WORK_DIR/BUCK" ]; then
        cat "$WORK_DIR/BUCK" > /dev/null 2>&1 && echo "✓ 文件读取成功" || echo "✗ 文件读取失败"
    fi
else
    echo "错误: 工作目录不存在: $WORK_DIR"
    echo "可用的目录:"
    find "$MOUNT_POINT" -maxdepth 2 -type d 2>/dev/null | head -10
fi

echo ""
echo "=========================================="
echo "步骤 5: 尝试 Buck2 编译"
echo "=========================================="

if [ ! -d "$WORK_DIR" ]; then
    echo "跳过 Buck2 编译（工作目录不存在）"
else
    # 设置 Buck2 环境变量（尝试避免 SQLite SHM 问题）
    export BUCK2_DAEMON_DIR=/tmp/buck2_daemon_test
    export BUCK2_ISOLATION_DIR=/tmp/buck2_daemon_test/isolation
    export TMPDIR=/tmp/buck2_daemon_test/tmp
    export BUCK_OUT=/tmp/buck2_daemon_test/buck-out
    export HOME=/root
    export BUCK2_ALLOW_ROOT=1

    mkdir -p "$BUCK2_DAEMON_DIR" "$BUCK2_ISOLATION_DIR" "$TMPDIR" "$BUCK_OUT"

    cd "$WORK_DIR"

    echo "当前目录: $(pwd)"
    echo "Buck2 环境变量:"
    echo "  BUCK2_DAEMON_DIR=$BUCK2_DAEMON_DIR"
    echo "  BUCK2_ISOLATION_DIR=$BUCK2_ISOLATION_DIR"
    echo "  TMPDIR=$TMPDIR"
    echo "  BUCK_OUT=$BUCK_OUT"
    echo ""

    # 检查 buck2 是否可用
    if ! command -v buck2 &> /dev/null; then
        echo "错误: buck2 命令未找到"
        echo "请确保 buck2 已安装并在 PATH 中"
    else
        echo "Buck2 版本:"
        buck2 --version
        echo ""

        echo "尝试运行: buck2 build //..."
        echo "----------------------------------------"
        buck2 build //... 2>&1 | tee "$LOG_DIR/buck2_build.log" || {
            BUILD_EXIT_CODE=$?
            echo ""
            echo "=========================================="
            echo "Buck2 编译失败 (退出码: $BUILD_EXIT_CODE)"
            echo "=========================================="
            echo ""
            echo "错误日志已保存到: $LOG_DIR/buck2_build.log"
            echo ""
            echo "检查关键错误信息:"
            grep -i "error\|fail\|sqlite\|shm\|mmap\|5386" "$LOG_DIR/buck2_build.log" | head -20 || true
            echo ""
            
            # 检查是否有 SQLite 相关错误
            if grep -qi "sqlite\|shm\|mmap\|5386\|xShmMap" "$LOG_DIR/buck2_build.log"; then
                echo "⚠️  检测到 SQLite 相关错误（可能是 FUSE 共享内存问题）"
                echo ""
                echo "检查 Buck2 在工作目录中创建的文件:"
                find "$WORK_DIR" -name "*.db*" -o -name ".buck2" -type d 2>/dev/null | head -10
                echo ""
                echo "检查 Buck2 在挂载点根目录创建的文件:"
                find "$MOUNT_POINT" -name "*.db*" 2>/dev/null | head -10
            fi
        }
    fi
fi

echo ""
echo "=========================================="
echo "步骤 6: 检查文件分布"
echo "=========================================="

# 从配置中获取路径
UPPER_ROOT=$(grep "antares_upper_root" scorpio/scorpio.toml | cut -d'=' -f2 | tr -d ' "' || echo "/tmp/megadir/antares/upper")
CL_ROOT=$(grep "antares_cl_root" scorpio/scorpio.toml | cut -d'=' -f2 | tr -d ' "' || echo "/tmp/megadir/antares/cl")
STORE_PATH=$(grep "store_path" scorpio/scorpio.toml | cut -d'=' -f2 | tr -d ' "' || echo "/tmp/megadir/store")

# 查找实际的 upper 目录（antares 会创建子目录）
UPPER_DIR=$(find "$UPPER_ROOT" -type d -name "*" 2>/dev/null | grep -v "^$UPPER_ROOT$" | head -1 || echo "")

echo "配置路径:"
echo "  Upper Root: $UPPER_ROOT"
echo "  CL Root: $CL_ROOT"
echo "  Store Path: $STORE_PATH"
echo ""

if [ -n "$UPPER_DIR" ]; then
    echo "upper/ 目录内容（用户修改的文件）:"
    find "$UPPER_DIR" -type f 2>/dev/null | head -20 || echo "  (空)"
    echo ""
else
    echo "upper/ 目录: (未找到或为空)"
    echo ""
fi

echo "store/ 目录内容（Dicfuse 缓存）:"
if [ -d "$STORE_PATH" ]; then
    ls -la "$STORE_PATH" | head -20
    if [ -d "$STORE_PATH/db" ]; then
        echo "  数据库文件:"
        ls -lh "$STORE_PATH/db" 2>/dev/null | head -5 || true
    fi
else
    echo "  (不存在)"
fi
echo ""

echo "=========================================="
echo "步骤 7: 列出当前挂载"
echo "=========================================="
cargo run -p scorpio --bin antares -- --config-path scorpio/scorpio.toml list 2>&1 | tee "$LOG_DIR/list_after.log"

echo ""
echo "=========================================="
echo "测试完成"
echo "=========================================="
echo "日志文件:"
echo "  编译日志: $LOG_DIR/build.log"
echo "  挂载日志: $LOG_DIR/mount.log"
echo "  Buck2 日志: $LOG_DIR/buck2_build.log"
echo "  挂载列表: $LOG_DIR/list_after.log"
echo ""
echo "挂载信息:"
echo "  Job ID: $JOB_ID"
echo "  挂载点: $MOUNT_POINT"
echo ""
echo "清理命令:"
echo "  cargo run -p scorpio --bin antares -- --config-path scorpio/scorpio.toml umount $JOB_ID"
echo ""
