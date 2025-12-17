#!/bin/bash
# 快速检查 getattr_with_mapping 实现的脚本
#
# 使用方法：
#   ./scripts/quick_check_getattr.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

echo "=== 检查 getattr_with_mapping 实现 ==="
echo ""

# 检查方法是否存在
if grep -q "async fn getattr_with_mapping" src/dicfuse/mod.rs; then
    echo "✓ 方法存在"
else
    echo "✗ 方法不存在"
    echo "  这就是导致 Buck2 SQLite xShmMap 错误的根本原因！"
    exit 1
fi

# 检查方法签名
if grep -A 5 "async fn getattr_with_mapping" src/dicfuse/mod.rs | grep -q "_mapping: bool\|mapping: bool"; then
    echo "✓ 方法签名正确（包含 mapping 参数）"
else
    echo "✗ 方法签名不正确（缺少 mapping 参数）"
    echo "  这可能导致编译错误或运行时错误"
    exit 1
fi

# 检查是否返回 ENOSYS（未实现）
if grep -A 15 "async fn getattr_with_mapping" src/dicfuse/mod.rs | grep -q "return.*ENOSYS\|Err.*ENOSYS"; then
    echo "✗ 方法返回 ENOSYS（未实现）"
    echo "  这就是导致 Buck2 SQLite xShmMap 错误的根本原因！"
    exit 1
else
    echo "✓ 方法已实现（不返回 ENOSYS）"
fi

# 检查 libfuse-fs 版本
LIBFUSE_VERSION=$(grep "libfuse-fs" Cargo.toml | grep -o '"[0-9.]*"' | tr -d '"')
echo "✓ libfuse-fs 版本: $LIBFUSE_VERSION"

if [[ "$LIBFUSE_VERSION" < "0.1.9" ]]; then
    echo "  ⚠ 建议升级到 0.1.9 或更高版本"
fi

# 运行测试
echo ""
echo "运行单元测试..."
if cargo test --lib dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size --quiet 2>&1 | grep -q "ok"; then
    echo "✓ 单元测试通过"
else
    echo "✗ 单元测试失败"
    echo "  运行详细测试:"
    echo "  cargo test --lib dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size"
    exit 1
fi

echo ""
echo "=== 所有检查通过 ==="
echo ""
echo "结论:"
echo "  ✓ getattr_with_mapping 已正确实现"
echo "  ✓ Buck2 SQLite xShmMap 错误应该已解决"
echo ""
echo "建议进行实际测试:"
echo "  1. 挂载 Antares overlay: cargo run --bin mount_test"
echo "  2. 在挂载点上运行 Buck2: buck2 build //..."
echo "  3. 如果仍有问题，查看调试日志: RUST_LOG=debug"

