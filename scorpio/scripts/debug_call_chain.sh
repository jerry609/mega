#!/bin/bash
# 调试完整的调用链路，确认问题根源
#
# 使用方法：
#   sudo ./scripts/debug_call_chain.sh

set -e

echo "========================================="
echo "调试 OverlayFS Copy-up 调用链路"
echo "========================================="
echo ""

# 检查是否有 root 权限
if [ "$EUID" -ne 0 ]; then 
    echo "❌ 此脚本需要 root 权限"
    echo "请使用: sudo ./scripts/debug_call_chain.sh"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

echo "1. 准备测试环境..."
echo ""

# 创建临时目录
TEST_DIR="/tmp/debug_call_chain_$$"
mkdir -p "$TEST_DIR"/{upper,lower,work,mnt}

echo "  测试目录: $TEST_DIR"
echo "  - upper: $TEST_DIR/upper"
echo "  - lower: $TEST_DIR/lower"
echo "  - work: $TEST_DIR/work"
echo "  - mnt: $TEST_DIR/mnt"
echo ""

# 清理函数
cleanup() {
    echo ""
    echo "清理测试环境..."
    
    # 尝试卸载
    if mountpoint -q "$TEST_DIR/mnt" 2>/dev/null; then
        fusermount3 -u "$TEST_DIR/mnt" 2>/dev/null || umount -f "$TEST_DIR/mnt" 2>/dev/null || true
        sleep 1
    fi
    
    # 删除临时目录
    rm -rf "$TEST_DIR"
    
    echo "清理完成"
}

trap cleanup EXIT INT TERM

echo "========================================="
echo "2. 创建测试文件..."
echo ""

# 在 lower layer 创建一个测试文件
echo "Hello from lower layer" > "$TEST_DIR/lower/test.txt"
chmod 644 "$TEST_DIR/lower/test.txt"

echo "  创建文件: $TEST_DIR/lower/test.txt"
echo "  内容: $(cat $TEST_DIR/lower/test.txt)"
echo ""

echo "========================================="
echo "3. 启动 Antares 挂载（带详细日志）..."
echo ""

# 设置环境变量启用详细日志
export RUST_LOG="scorpio=debug,libfuse_fs=debug"
export RUST_BACKTRACE=1

# 构建 scorpio（如果需要）
if [ ! -f "target/debug/scorpio" ]; then
    echo "  构建 scorpio..."
    cargo build 2>&1 | tail -5
    echo ""
fi

echo "  启动 Antares 挂载..."
echo "  日志级别: RUST_LOG=$RUST_LOG"
echo ""

# 创建配置文件
cat > "$TEST_DIR/config.toml" <<EOF
[mount]
mountpoint = "$TEST_DIR/mnt"
upper_dir = "$TEST_DIR/upper"
lower_dir = "$TEST_DIR/lower"
work_dir = "$TEST_DIR/work"

[store]
path = "$TEST_DIR/store"
EOF

# 后台启动 Antares（如果有的话）
# 否则使用标准的 overlayfs
echo "  注意: 此脚本使用标准 overlayfs 来演示调用链路"
echo "  实际的 Antares 挂载会有类似的行为"
echo ""

# 使用标准 overlayfs 进行演示
mount -t overlay overlay \
    -o lowerdir="$TEST_DIR/lower",upperdir="$TEST_DIR/upper",workdir="$TEST_DIR/work" \
    "$TEST_DIR/mnt"

echo "  ✓ overlayfs 挂载成功"
echo ""

echo "========================================="
echo "4. 测试场景 1: 读取文件（不触发 copy-up）..."
echo ""

echo "  读取 lower layer 的文件："
cat "$TEST_DIR/mnt/test.txt"
echo ""

echo "  检查 upper layer:"
if [ -f "$TEST_DIR/upper/test.txt" ]; then
    echo "  ✗ 文件已经 copy-up（不应该）"
else
    echo "  ✓ 文件未 copy-up（符合预期）"
fi
echo ""

echo "========================================="
echo "5. 测试场景 2: 写入文件（触发 copy-up）..."
echo ""

echo "  尝试写入文件（触发 copy-up）..."

# 启用 strace 追踪系统调用
echo "  使用 strace 追踪系统调用..."
strace -f -e trace=getxattr,stat,lstat,fstat,open,openat,create,write \
    sh -c "echo 'Modified' >> $TEST_DIR/mnt/test.txt" 2>&1 | \
    grep -E "getxattr|stat|open|write" | head -20 || true

echo ""

echo "  检查 upper layer:"
if [ -f "$TEST_DIR/upper/test.txt" ]; then
    echo "  ✓ 文件已经 copy-up"
    echo "  内容:"
    cat "$TEST_DIR/upper/test.txt" | sed 's/^/    /'
else
    echo "  ✗ 文件未 copy-up（不符合预期）"
fi
echo ""

echo "========================================="
echo "6. 测试场景 3: 创建新文件..."
echo ""

echo "  创建新文件..."
echo "New file" > "$TEST_DIR/mnt/newfile.txt"

echo "  检查文件位置:"
if [ -f "$TEST_DIR/upper/newfile.txt" ]; then
    echo "  ✓ 文件在 upper layer"
elif [ -f "$TEST_DIR/lower/newfile.txt" ]; then
    echo "  ✗ 文件在 lower layer（不应该）"
else
    echo "  ✗ 文件未创建"
fi
echo ""

echo "========================================="
echo "7. 模拟 Buck2 场景: 创建 SQLite 数据库..."
echo ""

# 检查是否有 sqlite3
if command -v sqlite3 &> /dev/null; then
    echo "  创建 SQLite 数据库..."
    
    # 尝试创建数据库（这会触发 copy-up）
    sqlite3 "$TEST_DIR/mnt/test.db" "CREATE TABLE test (id INTEGER PRIMARY KEY, value TEXT);" 2>&1 || {
        echo "  ✗ SQLite 数据库创建失败"
        echo "  这可能是 xShmMap 错误的原因"
    }
    
    if [ -f "$TEST_DIR/mnt/test.db" ]; then
        echo "  ✓ SQLite 数据库创建成功"
        
        # 检查是否有 WAL 文件
        if [ -f "$TEST_DIR/mnt/test.db-shm" ] || [ -f "$TEST_DIR/mnt/test.db-wal" ]; then
            echo "  ✓ WAL 模式文件创建成功"
        else
            echo "  ⚠ 未发现 WAL 模式文件"
        fi
        
        echo ""
        echo "  数据库文件列表:"
        ls -lh "$TEST_DIR/mnt"/test.db* | sed 's/^/    /'
    fi
else
    echo "  ⚠ sqlite3 未安装，跳过此测试"
fi

echo ""

echo "========================================="
echo "8. 分析调用链路..."
echo ""

echo "关键调用链路（标准 OverlayFS）:"
echo ""
echo "用户操作: echo 'Modified' >> /mnt/test.txt"
echo "  │"
echo "  ▼"
echo "VFS: sys_write()"
echo "  │"
echo "  ▼"
echo "OverlayFS: ovl_write_iter()"
echo "  │"
echo "  ├─ 检查文件是否在 upper layer"
echo "  │  └─ 不在 → 需要 copy-up"
echo "  │"
echo "  ▼"
echo "OverlayFS: ovl_copy_up()"
echo "  │"
echo "  ├─ 获取 lower layer 的文件属性"
echo "  │  └─ vfs_getattr() / vfs_fstat()"
echo "  │      │"
echo "  │      └─ 📍 关键点：需要获取准确的文件大小、权限、所有者"
echo "  │"
echo "  ├─ 在 upper layer 创建文件"
echo "  │  └─ vfs_create()"
echo "  │"
echo "  ├─ 复制文件内容"
echo "  │  └─ vfs_read() + vfs_write()"
echo "  │"
echo "  └─ 复制扩展属性（xattr）"
echo "     └─ vfs_getxattr() + vfs_setxattr()"
echo ""

echo "在 Antares/Dicfuse 场景中的调用链路："
echo ""
echo "用户操作: touch /mnt/test.txt"
echo "  │"
echo "  ▼"
echo "FUSE 内核: FUSE_CREATE 请求"
echo "  │"
echo "  ▼"
echo "OverlayFS (libfuse-fs)::create()"
echo "  │"
echo "  ├─ 检查文件是否在 lower layer"
echo "  │  └─ 在 → 需要 copy-up"
echo "  │"
echo "  ▼"
echo "OverlayFS::copy_node_up()"
echo "  │"
echo "  ├─ 对于目录: create_upper_dir()"
echo "  │  └─ 📍 lower_layer.do_getattr_helper() (0.1.8)"
echo "  │      或 lower_layer.getattr_with_mapping(..., false) (0.1.9)"
echo "  │"
echo "  └─ 对于文件: copy_regfile_up()"
echo "     │"
echo "     ├─ 📍 lower_layer.do_getattr_helper() (0.1.8)"
echo "     │   或 lower_layer.getattr_with_mapping(..., false) (0.1.9)"
echo "     │   │"
echo "     │   └─ Dicfuse::do_getattr_helper() / getattr_with_mapping()"
echo "     │       │"
echo "     │       ├─ ✅ 如果已实现: 返回正确的 stat 信息"
echo "     │       │   └─ copy-up 成功"
echo "     │       │"
echo "     │       └─ ❌ 如果未实现: 返回 ENOSYS"
echo "     │           └─ copy-up 失败"
echo "     │               └─ 文件创建失败"
echo "     │                   └─ SQLite xShmMap 错误"
echo "     │"
echo "     ├─ 在 upper layer 创建文件"
echo "     │  └─ upper_layer.create_with_context()"
echo "     │"
echo "     └─ 复制文件内容"
echo "        └─ lower_layer.read() + upper_layer.write()"
echo ""

echo "========================================="
echo "9. 总结..."
echo ""

echo "✅ 调试完成"
echo ""
echo "关键发现："
echo "  1. OverlayFS copy-up 需要获取 lower layer 的文件属性"
echo "  2. 在 libfuse-fs 中，这通过 Layer trait 的方法实现："
echo "     - 0.1.8: do_getattr_helper()"
echo "     - 0.1.9: getattr_with_mapping()"
echo "  3. 如果 Dicfuse 未实现这些方法，会返回 ENOSYS"
echo "  4. ENOSYS 导致 copy-up 失败，进而导致文件创建失败"
echo "  5. SQLite 在 WAL 模式下需要创建 .shm 文件，失败时报 xShmMap 错误"
echo ""

echo "验证方法："
echo "  1. 检查 Dicfuse 是否实现了相应的方法"
echo "  2. 启用 RUST_LOG=debug 查看详细日志"
echo "  3. 使用 strace 追踪系统调用"
echo ""

echo "========================================="

