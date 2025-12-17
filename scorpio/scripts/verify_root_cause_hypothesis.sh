#!/bin/bash
# 验证根本原因假设
#
# 使用方法：
#   ./scripts/verify_root_cause_hypothesis.sh

set -e

echo "========================================="
echo "验证根本原因假设"
echo "========================================="
echo ""

# 1. 验证：0.1.8 和 0.1.9 的 copy_regfile_up 实现是否相同？
echo "1. 对比 copy_regfile_up 实现..."
echo ""

V8_FILE="$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libfuse-fs-0.1.8/src/unionfs/mod.rs"
V9_FILE="$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libfuse-fs-0.1.9/src/unionfs/mod.rs"

if [ ! -f "$V8_FILE" ] || [ ! -f "$V9_FILE" ]; then
    echo "❌ 无法找到 libfuse-fs 源文件"
    exit 1
fi

# 提取 copy_regfile_up 方法
V8_START=$(grep -n "async fn copy_regfile_up" "$V8_FILE" | head -1 | cut -d: -f1)
V8_END=$(tail -n +$V8_START "$V8_FILE" | grep -n "^    }" | head -1 | cut -d: -f1)
V8_END=$((V8_START + V8_END - 1))

V9_START=$(grep -n "async fn copy_regfile_up" "$V9_FILE" | head -1 | cut -d: -f1)
V9_END=$(tail -n +$V9_START "$V9_FILE" | grep -n "^    }" | head -1 | cut -d: -f1)
V9_END=$((V9_START + V9_END - 1))

echo "0.1.8 版本: 行 $V8_START - $V8_END"
echo "0.1.9 版本: 行 $V9_START - $V9_END"
echo ""

# 提取并保存
sed -n "${V8_START},${V8_END}p" "$V8_FILE" > /tmp/v8_copy_regfile_up.rs
sed -n "${V9_START},${V9_END}p" "$V9_FILE" > /tmp/v9_copy_regfile_up.rs

# 对比（忽略空白差异）
DIFF_COUNT=$(diff -B -w /tmp/v8_copy_regfile_up.rs /tmp/v9_copy_regfile_up.rs | grep -E "^<|^>" | wc -l)

if [ "$DIFF_COUNT" -eq 0 ]; then
    echo "✅ 两个版本的实现完全相同（忽略空白）"
else
    echo "⚠️ 两个版本有 $DIFF_COUNT 行差异"
    echo ""
    echo "主要差异："
    diff -B -w /tmp/v8_copy_regfile_up.rs /tmp/v9_copy_regfile_up.rs | grep -E "^<|^>" | head -10
fi

echo ""
echo "========================================="

# 2. 验证：是否调用了 do_getattr_helper / getattr_with_mapping？
echo "2. 检查方法调用..."
echo ""

V8_CALL=$(grep -o "do_getattr_helper" /tmp/v8_copy_regfile_up.rs | wc -l)
V9_CALL=$(grep -o "getattr_with_mapping" /tmp/v9_copy_regfile_up.rs | wc -l)

echo "0.1.8 版本调用 do_getattr_helper: $V8_CALL 次"
echo "0.1.9 版本调用 getattr_with_mapping: $V9_CALL 次"

if [ "$V8_CALL" -gt 0 ] && [ "$V9_CALL" -gt 0 ]; then
    echo "✅ 两个版本都调用了相应的方法"
else
    echo "❌ 存在版本没有调用方法"
fi

echo ""
echo "========================================="

# 3. 验证：文件复制逻辑是否相同？
echo "3. 检查文件复制逻辑..."
echo ""

# 检查是否有 "not deal with it" 注释
V8_TODO=$(grep -c "not deal\|TODO\|FIXME" /tmp/v8_copy_regfile_up.rs || echo "0")
V9_TODO=$(grep -c "not deal\|TODO\|FIXME" /tmp/v9_copy_regfile_up.rs || echo "0")

echo "0.1.8 版本的 TODO/FIXME 注释: $V8_TODO 个"
echo "0.1.9 版本的 TODO/FIXME 注释: $V9_TODO 个"

if [ "$V8_TODO" -gt 0 ]; then
    echo ""
    echo "0.1.8 版本的未完成代码标记："
    grep -n "not deal\|TODO\|FIXME" /tmp/v8_copy_regfile_up.rs | sed 's/^/  /'
fi

if [ "$V9_TODO" -gt 0 ]; then
    echo ""
    echo "0.1.9 版本的未完成代码标记："
    grep -n "not deal\|TODO\|FIXME" /tmp/v9_copy_regfile_up.rs | sed 's/^/  /'
fi

echo ""

if [ "$V8_TODO" -eq "$V9_TODO" ]; then
    echo "⚠️ 两个版本的 TODO 数量相同 - 文件复制逻辑可能都未完成"
else
    echo "⚠️ 两个版本的 TODO 数量不同"
fi

echo ""
echo "========================================="

# 4. 验证：0.1.8 和 0.1.9 的 Layer trait 定义
echo "4. 检查 Layer trait 定义..."
echo ""

V8_LAYER="$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libfuse-fs-0.1.8/src/unionfs/layer.rs"
V9_LAYER="$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libfuse-fs-0.1.9/src/unionfs/layer.rs"

V8_HAS_OLD=$(grep -c "do_getattr_helper" "$V8_LAYER" || echo "0")
V8_HAS_NEW=$(grep -c "getattr_with_mapping" "$V8_LAYER" || echo "0")
V9_HAS_OLD=$(grep -c "do_getattr_helper" "$V9_LAYER" || echo "0")
V9_HAS_NEW=$(grep -c "getattr_with_mapping" "$V9_LAYER" || echo "0")

echo "0.1.8 Layer trait:"
echo "  - do_getattr_helper: $V8_HAS_OLD"
echo "  - getattr_with_mapping: $V8_HAS_NEW"
echo ""
echo "0.1.9 Layer trait:"
echo "  - do_getattr_helper: $V9_HAS_OLD"
echo "  - getattr_with_mapping: $V9_HAS_NEW"
echo ""

if [ "$V8_HAS_OLD" -gt 0 ] && [ "$V9_HAS_NEW" -gt 0 ]; then
    echo "✅ 0.1.8 有 do_getattr_helper，0.1.9 有 getattr_with_mapping"
else
    echo "⚠️ API 定义不符合预期"
fi

echo ""
echo "========================================="

# 5. 验证：默认实现是否返回 ENOSYS？
echo "5. 检查默认实现..."
echo ""

V8_DEFAULT=$(grep -A 5 "async fn do_getattr_helper" "$V8_LAYER" | grep -c "ENOSYS" || echo "0")
V9_DEFAULT=$(grep -A 5 "async fn getattr_with_mapping" "$V9_LAYER" | grep -c "ENOSYS" || echo "0")

echo "0.1.8 do_getattr_helper 默认返回 ENOSYS: $([ "$V8_DEFAULT" -gt 0 ] && echo "✅ 是" || echo "❌ 否")"
echo "0.1.9 getattr_with_mapping 默认返回 ENOSYS: $([ "$V9_DEFAULT" -gt 0 ] && echo "✅ 是" || echo "❌ 否")"

echo ""
echo "========================================="

# 6. 总结
echo "6. 验证总结"
echo ""

echo "关键发现："
echo ""

# 判断根本原因
if [ "$DIFF_COUNT" -le 10 ]; then
    echo "✅ copy_regfile_up 实现基本相同（差异 <= 10 行）"
    echo "   → 问题不在于实现逻辑本身"
else
    echo "⚠️ copy_regfile_up 实现有显著差异（差异 > 10 行）"
    echo "   → 可能存在实现 bug 修复"
fi

echo ""

if [ "$V8_TODO" -gt 0 ]; then
    echo "⚠️ 0.1.8 版本有未完成的代码（TODO/FIXME）"
    echo "   → 但这些代码在 0.1.9 中仍然存在"
    echo "   → 所以不是根本原因"
else
    echo "✅ 没有发现明显的未完成代码"
fi

echo ""

if [ "$V8_HAS_OLD" -gt 0 ] && [ "$V9_HAS_NEW" -gt 0 ]; then
    echo "✅ API 变更确认："
    echo "   - 0.1.8: do_getattr_helper"
    echo "   - 0.1.9: getattr_with_mapping"
    echo "   → 这是主要的 API 变更"
else
    echo "❌ API 变更不符合预期"
fi

echo ""

if [ "$V8_DEFAULT" -gt 0 ] && [ "$V9_DEFAULT" -gt 0 ]; then
    echo "✅ 两个版本的默认实现都返回 ENOSYS"
    echo "   → 如果 Dicfuse 未实现，都会失败"
    echo "   → 问题可能不在于版本差异"
else
    echo "⚠️ 默认实现不一致"
fi

echo ""
echo "========================================="
echo "最可能的结论："
echo ""
echo "基于验证结果，最可能的原因是："
echo ""
echo "1. 0.1.8 和 0.1.9 的 copy_regfile_up 实现基本相同"
echo "2. 两个版本都有未完成的 TODO（文件复制逻辑）"
echo "3. 主要差异是 API 变更：do_getattr_helper → getattr_with_mapping"
echo "4. 两个版本的默认实现都返回 ENOSYS"
echo ""
echo "因此，问题可能在于："
echo "  ⚠️ 0.1.8 版本在某些场景下不调用 do_getattr_helper"
echo "  ⚠️ 0.1.8 版本的 unionfs 和 overlayfs 不一致"
echo "  ⚠️ 0.1.9 版本修复了这些不一致问题（PR #335）"
echo ""
echo "需要进一步验证："
echo "  - 检查 0.1.8 是否有其他代码路径不调用 do_getattr_helper"
echo "  - 查看 PR #335 的具体修复内容"
echo "  - 测试 0.1.8 版本，启用详细日志确认失败位置"
echo ""
echo "========================================="

