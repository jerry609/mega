#!/bin/bash
# 复现 Buck2 SQLite xShmMap 错误的脚本
#
# 这个脚本通过临时禁用 getattr_with_mapping 来复现问题
# 用于验证问题确实是由该方法缺失导致的
#
# 使用方法：
#   ./scripts/reproduce_xshmmap_error.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

MOD_RS="src/dicfuse/mod.rs"
BACKUP_RS="src/dicfuse/mod.rs.backup_reproduce"

echo "=== 复现 Buck2 SQLite xShmMap 错误 ==="
echo ""
echo "警告: 这个脚本会临时修改代码来复现问题"
echo ""

# 检查是否已经有备份
if [ -f "$BACKUP_RS" ]; then
    echo "检测到已有备份文件，可能之前运行过此脚本"
    read -p "是否继续？(y/N) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        exit 1
    fi
fi

# 备份原始文件
echo "1. 备份原始文件..."
cp "$MOD_RS" "$BACKUP_RS"
echo "   ✓ 备份完成: $BACKUP_RS"

# 检查方法是否存在
if ! grep -q "async fn getattr_with_mapping" "$MOD_RS"; then
    echo ""
    echo "✗ 错误: 未找到 getattr_with_mapping 方法"
    echo "  可能已经禁用了，或者文件有问题"
    exit 1
fi

# 临时禁用方法
echo ""
echo "2. 临时禁用 getattr_with_mapping 方法..."

# 使用 sed 在方法开头添加返回 ENOSYS
python3 << 'PYTHON_SCRIPT'
import re
import sys

file_path = sys.argv[1]

with open(file_path, 'r') as f:
    content = f.read()

# 查找 getattr_with_mapping 方法
pattern = r'(async fn getattr_with_mapping\([^)]+\)[^{]*\{)(.*?)(^\s*\}\s*$)'

def replace_method(match):
    sig = match.group(1)
    body = match.group(2)
    closing = match.group(3)
    
    # 检查是否已经禁用了
    if "REPRODUCE_XSHMMAP_ERROR" in body:
        print("   ⚠ 方法似乎已经被禁用")
        return match.group(0)
    
    # 替换实现为返回 ENOSYS
    new_body = """
        // REPRODUCE_XSHMMAP_ERROR: 临时禁用以复现问题
        tracing::warn!("[REPRODUCE] getattr_with_mapping disabled for testing");
        return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
    """
    
    return sig + new_body + closing

# 使用多行模式
content_new = re.sub(pattern, replace_method, content, flags=re.MULTILINE | re.DOTALL)

if content_new != content:
    with open(file_path, 'w') as f:
        f.write(content_new)
    print("   ✓ 方法已禁用")
else:
    print("   ⚠ 未找到方法或已经禁用")
    sys.exit(1)
PYTHON_SCRIPT
"$MOD_RS"

if [ $? -ne 0 ]; then
    echo "   ✗ 修改失败"
    echo "   恢复备份..."
    cp "$BACKUP_RS" "$MOD_RS"
    exit 1
fi

echo ""
echo "3. 重新编译..."
if cargo build --quiet 2>&1; then
    echo "   ✓ 编译成功"
else
    echo "   ✗ 编译失败"
    echo "   恢复备份..."
    cp "$BACKUP_RS" "$MOD_RS"
    exit 1
fi

echo ""
echo "=== 准备就绪 ==="
echo ""
echo "现在可以测试 Buck2 构建来复现错误:"
echo ""
echo "1. 挂载 Antares overlay:"
echo "   cargo run --bin mount_test -- --config-path scorpio.toml"
echo ""
echo "2. 在另一个终端，在挂载点上运行 Buck2:"
echo "   cd /tmp/antares_test_*/mnt/third-party/buck-hello"
echo "   buck2 build //..."
echo ""
echo "3. 应该会看到 SQLite xShmMap 错误"
echo ""
read -p "是否现在恢复原始实现？(Y/n) " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Nn]$ ]]; then
    echo ""
    echo "4. 恢复原始实现..."
    cp "$BACKUP_RS" "$MOD_RS"
    echo "   ✓ 已恢复"
    
    echo ""
    echo "5. 重新编译..."
    cargo build --quiet 2>&1
    echo "   ✓ 编译完成"
    
    echo ""
    echo "=== 完成 ==="
    echo "原始实现已恢复，问题应该已解决"
fi

