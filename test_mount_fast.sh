#!/bin/bash

# 快速挂载测试 - 使用减少的加载深度

set -e

export RUSTUP_HOME=/home/master1/.rustup
export CARGO_HOME=/home/master1/.cargo
export CARGO_TARGET_DIR=/home/master1/mega/target-user
export PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig
export PATH="$CARGO_HOME/bin:$PATH"
export RUST_LOG="info,scorpio=info"

cd /home/master1/mega

echo "=========================================="
echo "快速挂载测试（减少加载深度）"
echo "=========================================="
echo ""
echo "使用 load_dir_depth=2 来加快速度"
echo "如果需要访问更深层的目录，可以增加这个值"
echo ""

# 终止旧进程
echo "清理旧进程..."
sudo pkill -f "mount_test" 2>/dev/null || true
sleep 2

# 清理旧目录
echo "清理旧测试目录..."
find /tmp -maxdepth 2 -type d -name "antares_test_*" -exec sudo rm -rf {} + 2>/dev/null || true

echo "启动挂载测试..."
sudo -E /home/master1/.cargo/bin/cargo run -p scorpio --bin mount_test -- \
    --config-path scorpio/scorpio_test.toml \
    --keep-alive 300 \
    2>&1 | tee /tmp/mount_test_fast.log

