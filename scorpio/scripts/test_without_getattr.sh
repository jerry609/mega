#!/bin/bash
# 测试脚本：临时禁用 getattr_with_mapping 来验证问题
#
# 这个脚本会：
# 1. 备份当前的 mod.rs
# 2. 临时注释掉 getattr_with_mapping 实现
# 3. 编译并运行测试
# 4. 恢复原始文件
#
# 使用方法：
#   ./scripts/test_without_getattr.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
MOD_RS="$SCORPIO_DIR/src/dicfuse/mod.rs"
BACKUP_RS="$SCORPIO_DIR/src/dicfuse/mod.rs.backup_test"

echo "=== 测试：验证 getattr_with_mapping 的重要性 ==="
echo ""

# 备份原始文件
echo "1. 备份原始 mod.rs..."
cp "$MOD_RS" "$BACKUP_RS"
echo "   ✓ 备份完成: $BACKUP_RS"

# 检查是否已经有注释标记
if grep -q "// TEST_DISABLE_getattr_with_mapping" "$MOD_RS"; then
    echo "   ℹ 检测到测试标记，跳过修改"
else
    echo ""
    echo "2. 临时注释掉 getattr_with_mapping 实现..."
    
    # 使用 sed 注释掉整个方法实现（保留方法签名但返回 ENOSYS）
    # 注意：这是一个简化的方法，实际应该更仔细地处理
    python3 << 'PYTHON_SCRIPT'
import re
import sys

file_path = sys.argv[1]
backup_path = sys.argv[2]

with open(file_path, 'r') as f:
    content = f.read()

# 查找 getattr_with_mapping 方法
pattern = r'(async fn getattr_with_mapping\([^)]+\)[^{]*\{)(.*?)(^\s*\}\s*$)'

def replace_method(match):
    sig = match.group(1)
    body = match.group(2)
    closing = match.group(3)
    
    # 替换实现为返回 ENOSYS
    new_body = """
        // TEST_DISABLE_getattr_with_mapping: 临时禁用以验证问题
        return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
    """
    
    return sig + new_body + closing

# 使用多行模式
content_new = re.sub(pattern, replace_method, content, flags=re.MULTILINE | re.DOTALL)

if content_new != content:
    with open(file_path, 'w') as f:
        f.write(content_new)
    print("   ✓ 已注释掉 getattr_with_mapping 实现")
else:
    print("   ⚠ 未找到 getattr_with_mapping 方法")
    sys.exit(1)
PYTHON_SCRIPT
    "$MOD_RS" "$BACKUP_RS"
    
    if [ $? -ne 0 ]; then
        echo "   ✗ 修改失败，使用手动方法"
        echo ""
        echo "   请手动编辑 $MOD_RS"
        echo "   在 getattr_with_mapping 方法开头添加："
        echo "   return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));"
        echo ""
        read -p "   按 Enter 继续..."
    fi
fi

echo ""
echo "3. 尝试编译..."
cd "$SCORPIO_DIR"
if cargo build --bin verify_getattr_issue 2>&1 | tee /tmp/verify_build.log; then
    echo "   ✓ 编译成功"
else
    echo "   ✗ 编译失败，查看 /tmp/verify_build.log"
    echo ""
    echo "4. 恢复原始文件..."
    cp "$BACKUP_RS" "$MOD_RS"
    exit 1
fi

echo ""
echo "4. 运行验证脚本（需要 root 权限）..."
echo "   注意：这个测试需要 FUSE 挂载，可能需要 root 权限"
echo ""
read -p "   是否继续运行测试？(y/N) " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    sudo -E cargo run --bin verify_getattr_issue 2>&1 | tee /tmp/verify_run.log
    echo ""
    echo "   查看 /tmp/verify_run.log 了解详细输出"
else
    echo "   跳过运行测试"
fi

echo ""
echo "5. 恢复原始文件..."
cp "$BACKUP_RS" "$MOD_RS"
echo "   ✓ 文件已恢复"

echo ""
echo "=== 测试完成 ==="
echo ""
echo "如果测试显示 ENOSYS 错误，说明 getattr_with_mapping 是必需的。"
echo "如果测试成功，说明当前实现正常工作。"

