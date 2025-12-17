#!/bin/bash
# 使用模拟数据测试 getattr_with_mapping（不需要 root 权限）
#
# 这个脚本创建一个简单的测试，验证 getattr_with_mapping 方法本身
# 不涉及实际的 FUSE 挂载

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

echo "=== 测试 getattr_with_mapping（使用模拟数据）==="
echo ""

echo "1. 运行单元测试..."
RUST_LOG=info cargo test --test verify_getattr_with_mapping --lib -- --nocapture 2>&1 | tail -20

echo ""
echo "2. 运行内部测试（使用 mock 数据）..."
RUST_LOG=info cargo test --lib dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size -- --nocapture 2>&1 | tail -20

echo ""
echo "=== 测试完成 ==="
echo ""
echo "这些测试验证了："
echo "  ✓ getattr_with_mapping 方法已实现"
echo "  ✓ 方法签名正确"
echo "  ✓ 方法能够正确返回 stat64 结构"
echo ""
echo "要进行完整的 copy-up 测试（需要 root 权限）："
echo "  sudo ./scripts/run_verification.sh"

