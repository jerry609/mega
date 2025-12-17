#!/bin/bash

# 快速挂载测试 - 使用已有缓存或等待加载完成

set -e

export RUSTUP_HOME=/home/master1/.rustup
export CARGO_HOME=/home/master1/.cargo
export CARGO_TARGET_DIR=/home/master1/mega/target-user
export PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig
export PATH="$CARGO_HOME/bin:$PATH"
export RUST_LOG="info,scorpio=info"

cd /home/master1/mega

echo "=========================================="
echo "快速挂载测试"
echo "=========================================="
echo ""
echo "注意: 首次运行需要从远端加载目录树，可能需要 5-15 分钟"
echo "      如果之前运行过，会使用缓存，速度会快很多"
echo ""

# 检查是否有旧的挂载进程
OLD_PID=$(ps aux | grep "mount_test" | grep -v grep | awk '{print $2}' | head -1)
if [ -n "$OLD_PID" ]; then
    echo "发现旧的挂载进程 (PID: $OLD_PID)"
    read -p "是否终止旧进程? (y/n) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        kill $OLD_PID 2>/dev/null && echo "已终止旧进程" || echo "无法终止"
    fi
fi

echo "启动挂载测试（后台运行）..."
nohup sudo -E /home/master1/.cargo/bin/cargo run -p scorpio --bin mount_test -- \
    --config-path scorpio/scorpio.toml \
    --keep-alive 600 \
    > /tmp/mount_test_output.log 2>&1 &
MOUNT_PID=$!

echo "挂载进程 PID: $MOUNT_PID"
echo "日志文件: /tmp/mount_test_output.log"
echo ""

# 等待并显示进度
echo "等待挂载完成（最多等待 5 分钟）..."
for i in {1..60}; do
    sleep 5
    if grep -q "挂载成功\|Mount completed" /tmp/mount_test_output.log 2>/dev/null; then
        echo ""
        echo "✓ 挂载成功！"
        break
    fi
    if ! ps -p $MOUNT_PID > /dev/null 2>&1; then
        echo ""
        echo "✗ 进程已退出，查看日志: /tmp/mount_test_output.log"
        tail -50 /tmp/mount_test_output.log
        exit 1
    fi
    echo -n "."
done
echo ""

# 查找挂载点
MOUNT_POINT=$(find /tmp -maxdepth 2 -type d -path "*/antares_test_*/mnt" 2>/dev/null | head -1)

if [ -n "$MOUNT_POINT" ] && mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    echo ""
    echo "=========================================="
    echo "挂载信息"
    echo "=========================================="
    echo "挂载点: $MOUNT_POINT"
    echo "进程 PID: $MOUNT_PID"
    echo ""
    echo "测试命令:"
    echo "  cd $MOUNT_POINT"
    echo "  ls -la"
    echo "  cd third-party/buck-hello"
    echo "  buck2 build //..."
    echo ""
    echo "查看日志:"
    echo "  tail -f /tmp/mount_test_output.log"
    echo ""
    echo "停止挂载:"
    echo "  kill $MOUNT_PID"
    echo ""
else
    echo ""
    echo "挂载可能还在进行中，查看日志:"
    echo "  tail -f /tmp/mount_test_output.log"
    echo ""
    tail -30 /tmp/mount_test_output.log
fi

