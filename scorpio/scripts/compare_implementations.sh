#!/bin/bash
# 对比 0.1.8 版本的 do_getattr_helper 和当前的 getattr_with_mapping 实现
#
# 使用方法：
#   ./scripts/compare_implementations.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

echo "=== 对比实现：do_getattr_helper vs getattr_with_mapping ==="
echo ""

# 获取移除前的实现（0.1.8 版本）
echo "1. 获取 0.1.8 版本的 do_getattr_helper 实现..."
OLD_IMPL=$(git show feaa21fc^:scorpio/src/dicfuse/mod.rs 2>/dev/null | grep -A 15 "async fn do_getattr_helper" | head -16)

if [ -z "$OLD_IMPL" ]; then
    echo "   ✗ 无法获取旧实现"
    exit 1
fi

echo "   ✓ 获取成功"
echo ""

# 获取当前的实现
echo "2. 获取当前的 getattr_with_mapping 实现..."
CURRENT_IMPL=$(grep -A 70 "async fn getattr_with_mapping" src/dicfuse/mod.rs | head -71)

if [ -z "$CURRENT_IMPL" ]; then
    echo "   ✗ 无法获取当前实现"
    exit 1
fi

echo "   ✓ 获取成功"
echo ""

# 保存到临时文件进行对比
TMP_OLD="/tmp/old_do_getattr_helper.rs"
TMP_CURRENT="/tmp/current_getattr_with_mapping.rs"

echo "$OLD_IMPL" > "$TMP_OLD"
echo "$CURRENT_IMPL" > "$TMP_CURRENT"

echo "3. 对比实现逻辑..."
echo ""

echo "--- 0.1.8 版本的 do_getattr_helper ---"
cat "$TMP_OLD"
echo ""

echo "--- 当前的 getattr_with_mapping ---"
cat "$TMP_CURRENT"
echo ""

echo "4. 分析差异..."
echo ""

# 提取核心逻辑（去除调试日志、参数差异等）
OLD_LOGIC=$(echo "$OLD_IMPL" | grep -E "(get_inode|get_stat|fileattr_to_stat64|Ok\(|Err\()" | tr -d ' ' | head -5)
CURRENT_LOGIC=$(echo "$CURRENT_IMPL" | grep -E "(get_inode|get_stat|stat64|Ok\(|Err\()" | tr -d ' ' | head -10)

echo "0.1.8 版本核心逻辑:"
echo "$OLD_LOGIC" | sed 's/^/  /'
echo ""

echo "当前版本核心逻辑:"
echo "$CURRENT_LOGIC" | sed 's/^/  /'
echo ""

# 检查是否都调用了 get_inode
if echo "$OLD_IMPL" | grep -q "get_inode" && echo "$CURRENT_IMPL" | grep -q "get_inode"; then
    echo "✓ 都调用了 get_inode"
else
    echo "✗ get_inode 调用不一致"
fi

# 检查是否都调用了 get_stat 或类似方法
if echo "$OLD_IMPL" | grep -q "get_stat" && echo "$CURRENT_IMPL" | grep -q "get_stat"; then
    echo "✓ 都调用了 get_stat"
else
    echo "✗ get_stat 调用不一致"
fi

# 检查返回类型
if echo "$OLD_IMPL" | grep -q "Ok((st," && echo "$CURRENT_IMPL" | grep -q "Ok((stat,"; then
    echo "✓ 都返回 Ok((stat, Duration))"
else
    echo "? 返回类型可能有差异"
fi

echo ""
echo "5. 关键差异分析..."
echo ""

# 检查函数签名差异
echo "函数签名差异:"
echo "  0.1.8: do_getattr_helper(inode, handle) -> Result<(stat64, Duration)>"
echo "  当前:  getattr_with_mapping(inode, handle, mapping) -> Result<(stat64, Duration)>"
echo "  差异: 新增 mapping: bool 参数"
echo ""

# 检查实现方式差异
if echo "$OLD_IMPL" | grep -q "fileattr_to_stat64"; then
    echo "0.1.8 版本: 使用 fileattr_to_stat64 辅助函数"
else
    echo "0.1.8 版本: 直接构造 stat64"
fi

if echo "$CURRENT_IMPL" | grep -q "fileattr_to_stat64"; then
    echo "当前版本: 使用 fileattr_to_stat64 辅助函数"
else
    echo "当前版本: 直接构造 stat64（内联实现）"
fi

echo ""
echo "6. 结论..."
echo ""

# 检查核心逻辑是否相同
OLD_CORE=$(echo "$OLD_IMPL" | grep -o "get_inode.*get_stat.*fileattr_to_stat64\|get_inode.*get_stat.*stat64" | head -1)
CURRENT_CORE=$(echo "$CURRENT_IMPL" | grep -o "get_inode.*get_stat.*stat64\|store.get_inode.*item.get_stat" | head -1)

if [ -n "$OLD_CORE" ] && [ -n "$CURRENT_CORE" ]; then
    echo "核心逻辑流程:"
    echo "  0.1.8: get_inode -> get_stat -> fileattr_to_stat64 -> Ok"
    echo "  当前:  get_inode -> get_stat -> 构造 stat64 -> Ok"
    echo ""
    echo "✓ 核心逻辑相同：都是从 store 获取 inode，然后获取 stat，最后构造 stat64"
    echo "✓ 只是实现方式略有不同（0.1.8 使用辅助函数，当前版本内联实现）"
else
    echo "? 需要进一步检查核心逻辑"
fi

echo ""
echo "=== 对比完成 ==="

