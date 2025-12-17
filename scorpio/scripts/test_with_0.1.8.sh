#!/bin/bash
# 切换到 libfuse-fs 0.1.8 并验证调用链路
#
# 使用方法：
#   ./scripts/test_with_0.1.8.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

echo "========================================="
echo "测试 libfuse-fs 0.1.8 版本"
echo "========================================="
echo ""

# 备份当前的 Cargo.toml
echo "1. 备份 Cargo.toml..."
cp Cargo.toml Cargo.toml.backup
echo "   ✓ 备份完成: Cargo.toml.backup"
echo ""

# 清理函数
cleanup() {
    echo ""
    echo "恢复环境..."
    if [ -f Cargo.toml.backup ]; then
        mv Cargo.toml.backup Cargo.toml
        echo "  ✓ 恢复 Cargo.toml"
    fi
    
    # 清理构建缓存
    echo "  清理构建缓存..."
    cargo clean 2>&1 | tail -3
}

trap cleanup EXIT INT TERM

echo "2. 修改 Cargo.toml 使用 libfuse-fs 0.1.8..."

# 修改 libfuse-fs 版本
sed -i 's/libfuse-fs = "0.1.9"/libfuse-fs = "0.1.8"/' Cargo.toml

# 验证修改
CURRENT_VERSION=$(grep 'libfuse-fs = ' Cargo.toml | head -1)
echo "   当前版本: $CURRENT_VERSION"
echo ""

echo "3. 检查当前 Dicfuse 是否有 do_getattr_helper 实现..."
echo ""

# 检查 mod.rs 中是否有 do_getattr_helper
if grep -q "async fn do_getattr_helper" src/dicfuse/mod.rs; then
    echo "   ✓ 发现 do_getattr_helper 实现"
    echo ""
    echo "   实现内容:"
    grep -A 20 "async fn do_getattr_helper" src/dicfuse/mod.rs | head -22 | sed 's/^/   /'
else
    echo "   ✗ 未发现 do_getattr_helper 实现"
    echo ""
    echo "   这证实了我们的假设："
    echo "   - 当前代码库中没有 do_getattr_helper 的实现"
    echo "   - 0.1.8 版本需要这个方法"
    echo "   - 所以会失败！"
fi

echo ""
echo "========================================="
echo "4. 尝试构建（预期会失败或使用默认实现）..."
echo ""

# 尝试构建
BUILD_OUTPUT=$(cargo build 2>&1 || true)

if echo "$BUILD_OUTPUT" | grep -q "error"; then
    echo "   ✗ 构建失败（预期）"
    echo ""
    echo "   错误信息:"
    echo "$BUILD_OUTPUT" | grep "error" | head -10 | sed 's/^/   /'
    echo ""
    echo "   这可能是因为:"
    echo "   - 0.1.8 和 0.1.9 的 API 不兼容"
    echo "   - 当前实现使用了 getattr_with_mapping (0.1.9 的方法)"
    echo "   - 0.1.8 需要 do_getattr_helper"
else
    echo "   ✓ 构建成功"
    echo ""
    echo "   这说明:"
    echo "   - 代码兼容 0.1.8"
    echo "   - 或者使用了默认实现"
    echo ""
    
    echo "5. 运行测试查看行为..."
    echo ""
    
    # 运行错误传播测试
    echo "   运行错误传播测试..."
    TEST_OUTPUT=$(cargo test --test test_copy_up_chain test_error_propagation_chain -- --nocapture 2>&1 || true)
    
    echo "$TEST_OUTPUT" | tail -30
fi

echo ""
echo "========================================="
echo "6. 分析结果..."
echo ""

echo "关键发现:"
echo ""

# 检查是否有 getattr_with_mapping
HAS_NEW_METHOD=$(grep -c "getattr_with_mapping" src/dicfuse/mod.rs || echo "0")
# 检查是否有 do_getattr_helper  
HAS_OLD_METHOD=$(grep -c "do_getattr_helper" src/dicfuse/mod.rs || echo "0")

echo "当前代码库:"
echo "  - getattr_with_mapping 实现: $HAS_NEW_METHOD 处"
echo "  - do_getattr_helper 实现: $HAS_OLD_METHOD 处"
echo ""

if [ "$HAS_NEW_METHOD" -gt 0 ] && [ "$HAS_OLD_METHOD" -eq 0 ]; then
    echo "结论:"
    echo "  ✓ 当前实现使用 getattr_with_mapping (0.1.9)"
    echo "  ✗ 没有 do_getattr_helper 实现 (0.1.8 需要)"
    echo ""
    echo "这解释了为什么:"
    echo "  1. 在 0.1.8 时代会失败（方法不匹配）"
    echo "  2. 升级到 0.1.9 后成功（API 匹配）"
elif [ "$HAS_OLD_METHOD" -gt 0 ] && [ "$HAS_NEW_METHOD" -eq 0 ]; then
    echo "结论:"
    echo "  ✓ 当前实现使用 do_getattr_helper (0.1.8)"
    echo "  ? 这不应该出现在当前版本"
elif [ "$HAS_NEW_METHOD" -gt 0 ] && [ "$HAS_OLD_METHOD" -gt 0 ]; then
    echo "结论:"
    echo "  ✓ 同时有两个版本的实现"
    echo "  这可能是过渡期的代码"
else
    echo "结论:"
    echo "  ✗ 两个方法都没有实现"
    echo "  这会导致使用默认实现（返回 ENOSYS）"
fi

echo ""
echo "========================================="
echo ""
echo "注意: 环境将在退出时自动恢复到 0.1.9"

