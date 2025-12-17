#!/bin/bash
# 运行验证脚本的便捷脚本
#
# 使用方法：
#   ./scripts/run_verification.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

echo "=== 运行 getattr_with_mapping 验证 ==="
echo ""

# 检查是否有 root 权限
if [ "$EUID" -ne 0 ]; then 
    echo "⚠ 需要 root 权限来挂载 FUSE 文件系统"
    echo ""
    echo "请使用以下命令运行："
    echo "  sudo -E $0"
    echo ""
    echo "或者："
    echo "  cd $SCORPIO_DIR"
    echo "  RUST_LOG=debug sudo -E cargo run --bin verify_getattr_issue"
    echo ""
    exit 1
fi

echo "1. 编译验证脚本..."
cargo build --bin verify_getattr_issue 2>&1 | tail -3

echo ""
echo "2. 运行验证脚本（启用 debug 日志）..."
echo "   这将："
echo "   - 创建 Dicfuse 实例"
echo "   - 挂载 Antares overlay"
echo "   - 尝试创建文件（触发 copy-up）"
echo "   - 显示 getattr_with_mapping 的调用日志"
echo ""

RUST_LOG=debug cargo run --bin verify_getattr_issue 2>&1 | tee /tmp/verify_getattr_output.log

echo ""
echo "=== 验证完成 ==="
echo ""
echo "日志已保存到: /tmp/verify_getattr_output.log"
echo ""
echo "查看 getattr_with_mapping 调用:"
echo "  grep 'getattr_with_mapping' /tmp/verify_getattr_output.log"
echo ""
echo "查看所有 Dicfuse 相关日志:"
echo "  grep 'Dicfuse' /tmp/verify_getattr_output.log"

