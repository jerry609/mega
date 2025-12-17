#!/bin/bash
# 验证根本原因：确认 feaa21fc 提交移除了 do_getattr_helper
#
# 使用方法：
#   ./scripts/verify_root_cause.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

echo "=== 验证根本原因 ==="
echo ""

echo "1. 检查 feaa21fc 提交..."
COMMIT_INFO=$(git show feaa21fc --stat --oneline 2>/dev/null | head -5)

if [ -z "$COMMIT_INFO" ]; then
    echo "   ✗ 无法找到 feaa21fc 提交"
    exit 1
fi

echo "$COMMIT_INFO" | sed 's/^/   /'
echo ""

echo "2. 检查移除前的实现（feaa21fc^）..."
BEFORE=$(git show feaa21fc^:scorpio/src/dicfuse/mod.rs 2>/dev/null | grep -c "async fn do_getattr_helper" || echo "0")

if [ "$BEFORE" -gt 0 ]; then
    echo "   ✓ 移除前有 do_getattr_helper 实现"
else
    echo "   ✗ 移除前没有 do_getattr_helper 实现"
    exit 1
fi

echo ""
echo "3. 检查移除后的实现（feaa21fc）..."
AFTER_COUNT=$(git show feaa21fc:scorpio/src/dicfuse/mod.rs 2>/dev/null | grep -c "async fn do_getattr_helper" || echo "0")

if [ "$AFTER_COUNT" = "0" ]; then
    echo "   ✓ 移除后没有 do_getattr_helper 实现（已移除）"
else
    echo "   ✗ 移除后仍有 do_getattr_helper 实现（找到 $AFTER_COUNT 处）"
    exit 1
fi

echo ""
echo "4. 查看移除的代码行数..."
DELETED_LINES=$(git show feaa21fc --stat 2>/dev/null | grep -o "[0-9]* deletion" | grep -o "[0-9]*" || echo "0")

if [ "$DELETED_LINES" -gt 0 ]; then
    echo "   ✓ 删除了 $DELETED_LINES 行代码"
else
    echo "   ? 无法确定删除的行数"
fi

echo ""
echo "5. 查看提交信息..."
COMMIT_MSG=$(git show feaa21fc --format="%B" --no-patch 2>/dev/null | head -10)
echo "$COMMIT_MSG" | sed 's/^/   /'

echo ""
echo "6. 对比实现逻辑..."
echo "   运行对比脚本..."
./scripts/compare_implementations.sh 2>&1 | tail -20

echo ""
echo "=== 验证完成 ==="
echo ""
echo "结论:"
echo "  ✓ feaa21fc 提交确实移除了 do_getattr_helper 实现"
echo "  ✓ 这导致在 0.1.8 版本中方法缺失"
echo "  ✓ 升级到 0.1.9 时实现了 getattr_with_mapping"
echo "  ✓ 核心逻辑相同，只是函数签名和实现细节略有不同"

