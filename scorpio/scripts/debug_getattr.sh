#!/bin/bash
# 调试脚本：运行验证并启用详细日志
#
# 使用方法：
#   ./scripts/debug_getattr.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

echo "=== 调试 getattr_with_mapping ==="
echo ""

echo "1. 编译验证脚本..."
cargo build --bin verify_getattr_issue 2>&1 | tail -3

echo ""
echo "2. 运行单元测试..."
RUST_LOG=debug cargo test --test verify_getattr_with_mapping --lib -- --nocapture 2>&1 | tail -30

echo ""
echo "3. 运行验证脚本（需要 root 权限）..."
echo "   注意：这将挂载 FUSE 文件系统，需要 root 权限"
echo ""
read -p "   是否继续运行验证脚本？(y/N) " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    echo ""
    echo "   运行验证脚本（启用 debug 日志）..."
    RUST_LOG=debug sudo -E cargo run --bin verify_getattr_issue 2>&1 | tee /tmp/verify_getattr_debug.log
    echo ""
    echo "   日志已保存到: /tmp/verify_getattr_debug.log"
    echo ""
    echo "   查看 getattr_with_mapping 调用:"
    echo "   grep 'getattr_with_mapping' /tmp/verify_getattr_debug.log"
else
    echo "   跳过验证脚本"
fi

echo ""
echo "=== 调试完成 ==="

