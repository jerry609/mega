#!/bin/bash
# 深入分析 libfuse-fs 源码，特别是 OverlayFS 的 copy-up 逻辑
#
# 这个脚本会：
# 1. 定位 libfuse-fs 源码位置
# 2. 提取关键代码片段
# 3. 分析 copy-up 调用链
# 4. 生成分析报告

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="$SCORPIO_DIR/doc/libfuse-source-analysis"

echo "=== libfuse-fs 源码深度分析 ==="
echo ""

# 查找 libfuse-fs 源码位置
LIBFUSE_09_SRC=$(find ~/.cargo/registry -path "*/libfuse-fs-0.1.9/src" -type d 2>/dev/null | head -1)
LIBFUSE_08_SRC=$(find ~/.cargo/registry -path "*/libfuse-fs-0.1.8/src" -type d 2>/dev/null | head -1)

if [ -z "$LIBFUSE_09_SRC" ]; then
    echo "错误: 未找到 libfuse-fs 0.1.9 源码"
    echo "请先运行: cargo build"
    exit 1
fi

echo "✓ 找到 libfuse-fs 0.1.9 源码: $LIBFUSE_09_SRC"
if [ -n "$LIBFUSE_08_SRC" ]; then
    echo "✓ 找到 libfuse-fs 0.1.8 源码: $LIBFUSE_08_SRC"
fi

mkdir -p "$OUTPUT_DIR"

echo ""
echo "1. 分析 Layer trait 定义..."
echo ""

# 提取 Layer trait 的 getattr_with_mapping 定义
echo "=== Layer trait: getattr_with_mapping 方法签名 ===" > "$OUTPUT_DIR/layer_trait_analysis.md"
echo "" >> "$OUTPUT_DIR/layer_trait_analysis.md"

if [ -f "$LIBFUSE_09_SRC/unionfs/layer.rs" ]; then
    echo "**0.1.9 版本** (libfuse-fs-0.1.9/src/unionfs/layer.rs):" >> "$OUTPUT_DIR/layer_trait_analysis.md"
    echo '```rust' >> "$OUTPUT_DIR/layer_trait_analysis.md"
    grep -A 10 "async fn getattr_with_mapping" "$LIBFUSE_09_SRC/unionfs/layer.rs" | head -12 >> "$OUTPUT_DIR/layer_trait_analysis.md"
    echo '```' >> "$OUTPUT_DIR/layer_trait_analysis.md"
    echo "" >> "$OUTPUT_DIR/layer_trait_analysis.md"
fi

if [ -n "$LIBFUSE_08_SRC" ] && [ -f "$LIBFUSE_08_SRC/unionfs/layer.rs" ]; then
    echo "**0.1.8 版本** (libfuse-fs-0.1.8/src/unionfs/layer.rs):" >> "$OUTPUT_DIR/layer_trait_analysis.md"
    echo '```rust' >> "$OUTPUT_DIR/layer_trait_analysis.md"
    grep -A 10 "async fn do_getattr_helper" "$LIBFUSE_08_SRC/unionfs/layer.rs" | head -12 >> "$OUTPUT_DIR/layer_trait_analysis.md"
    echo '```' >> "$OUTPUT_DIR/layer_trait_analysis.md"
    echo "" >> "$OUTPUT_DIR/layer_trait_analysis.md"
fi

echo "2. 分析 OverlayFS copy-up 操作..."
echo ""

# 提取 copy_regfile_up 方法
echo "=== OverlayFS::copy_regfile_up 方法 ===" > "$OUTPUT_DIR/copy_regfile_up_analysis.md"
echo "" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"

if [ -f "$LIBFUSE_09_SRC/unionfs/mod.rs" ]; then
    echo "**0.1.9 版本** (libfuse-fs-0.1.9/src/unionfs/mod.rs):" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
    echo "" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
    
    # 找到 copy_regfile_up 方法的行号
    LINE_NUM=$(grep -n "async fn copy_regfile_up" "$LIBFUSE_09_SRC/unionfs/mod.rs" | cut -d: -f1)
    if [ -n "$LINE_NUM" ]; then
        echo "方法位置: 第 $LINE_NUM 行" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
        echo "" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
        echo '```rust' >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
        # 提取方法签名和关键调用
        sed -n "${LINE_NUM},$((LINE_NUM+50))p" "$LIBFUSE_09_SRC/unionfs/mod.rs" | \
            grep -A 50 "async fn copy_regfile_up" | \
            head -40 >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
        echo '```' >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
    fi
fi

# 提取 create_upper_dir 方法
echo "" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
echo "=== OverlayFS::create_upper_dir 方法 ===" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
echo "" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"

if [ -f "$LIBFUSE_09_SRC/unionfs/mod.rs" ]; then
    LINE_NUM=$(grep -n "async fn create_upper_dir" "$LIBFUSE_09_SRC/unionfs/mod.rs" | cut -d: -f1)
    if [ -n "$LINE_NUM" ]; then
        echo "方法位置: 第 $LINE_NUM 行" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
        echo "" >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
        echo '```rust' >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
        sed -n "${LINE_NUM},$((LINE_NUM+30))p" "$LIBFUSE_09_SRC/unionfs/mod.rs" | \
            grep -A 30 "async fn create_upper_dir" | \
            head -25 >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
        echo '```' >> "$OUTPUT_DIR/copy_regfile_up_analysis.md"
    fi
fi

echo "3. 分析关键调用点..."
echo ""

# 查找所有调用 getattr_with_mapping 的地方
echo "=== getattr_with_mapping 调用点分析 ===" > "$OUTPUT_DIR/call_sites_analysis.md"
echo "" >> "$OUTPUT_DIR/call_sites_analysis.md"

if [ -f "$LIBFUSE_09_SRC/unionfs/mod.rs" ]; then
    echo "**在 OverlayFS 中的调用位置:**" >> "$OUTPUT_DIR/call_sites_analysis.md"
    echo "" >> "$OUTPUT_DIR/call_sites_analysis.md"
    
    grep -n "getattr_with_mapping" "$LIBFUSE_09_SRC/unionfs/mod.rs" | while read line; do
        LINE_NUM=$(echo "$line" | cut -d: -f1)
        CONTEXT=$(echo "$line" | cut -d: -f2-)
        echo "- 第 $LINE_NUM 行: $CONTEXT" >> "$OUTPUT_DIR/call_sites_analysis.md"
        
        # 提取上下文
        echo "  上下文:" >> "$OUTPUT_DIR/call_sites_analysis.md"
        echo '  ```rust' >> "$OUTPUT_DIR/call_sites_analysis.md"
        sed -n "$((LINE_NUM-5)),$((LINE_NUM+5))p" "$LIBFUSE_09_SRC/unionfs/mod.rs" | \
            sed 's/^/  /' >> "$OUTPUT_DIR/call_sites_analysis.md"
        echo '  ```' >> "$OUTPUT_DIR/call_sites_analysis.md"
        echo "" >> "$OUTPUT_DIR/call_sites_analysis.md"
    done
fi

echo "4. 生成调用链图..."
echo ""

cat > "$OUTPUT_DIR/call_chain_analysis.md" << 'EOF'
# OverlayFS Copy-up 调用链分析

## 完整调用链

```
用户操作: touch /mnt/path/to/file.txt
  │
  ▼
FUSE 内核: FUSE_CREATE 请求
  │
  ▼
OverlayFS::create (async_io.rs)
  │
  ├── 检查 upper layer 是否存在文件
  │   └── 不存在，需要 copy-up
  │
  └── OverlayFS::copy_node_up (mod.rs:2314)
      │
      ├── 对于目录: OverlayFS::create_upper_dir (mod.rs:723)
      │   │
      │   └── lower_layer.getattr_with_mapping(..., false)  ← 关键调用点 1
      │       │
      │       └── Dicfuse::getattr_with_mapping (如果未实现 → ENOSYS)
      │
      ├── 对于普通文件: OverlayFS::copy_regfile_up (mod.rs:2176)
      │   │
      │   └── lower_layer.getattr_with_mapping(..., false)  ← 关键调用点 2
      │       │
      │       └── Dicfuse::getattr_with_mapping (如果未实现 → ENOSYS)
      │
      └── 对于符号链接: OverlayFS::copy_symlink_up (mod.rs:2077)
          │
          └── lower_layer.getattr_with_mapping(..., false)  ← 关键调用点 3
              │
              └── Dicfuse::getattr_with_mapping (如果未实现 → ENOSYS)
```

## 关键代码位置

### 0.1.9 版本

1. **create_upper_dir** (mod.rs:723-742)
   - 调用: `lower_layer.getattr_with_mapping(self_inode, None, false)`
   - 目的: 获取目录的原始属性（UID/GID/mode）用于在 upper layer 创建目录

2. **copy_regfile_up** (mod.rs:2176-2200)
   - 调用: `lower_layer.getattr_with_mapping(lower_inode, None, false)`
   - 目的: 获取文件的原始属性（size/mode/UID/GID）用于在 upper layer 创建文件

3. **copy_symlink_up** (mod.rs:2077-2106)
   - 调用: `lower_layer.getattr_with_mapping(self_inode, None, false)`
   - 目的: 获取符号链接的原始属性用于在 upper layer 创建符号链接

## 错误传播路径

如果 `getattr_with_mapping` 未实现（返回 ENOSYS）：

```
OverlayFS::copy_regfile_up
  └── lower_layer.getattr_with_mapping(..., false)
      └── Layer trait 默认实现
          └── 返回 Err(ENOSYS)
              │
              ▼
copy_regfile_up 失败
              │
              ▼
OverlayFS::copy_node_up 失败
              │
              ▼
OverlayFS::create 失败
              │
              ▼
FUSE 返回错误给内核
              │
              ▼
系统调用 creat() 返回错误
              │
              ▼
SQLite 收到 I/O 错误
              │
              ▼
Buck2 报 "xShmMap I/O error"
```

## 验证方法

1. **检查方法是否存在**:
   ```rust
   use libfuse_fs::unionfs::layer::Layer;
   let result = dic.getattr_with_mapping(1, None, false).await;
   ```

2. **检查错误类型**:
   ```rust
   match result {
       Ok(_) => println!("方法已实现"),
       Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
           println!("方法未实现 (ENOSYS)")
       }
       Err(e) => println!("其他错误: {:?}", e),
   }
   ```

3. **测试 copy-up**:
   - 在挂载点上创建文件
   - 如果父目录在 lower layer，会触发 copy-up
   - 观察是否出现 ENOSYS 错误
EOF

echo "5. 对比 0.1.8 和 0.1.9 的差异..."
echo ""

cat > "$OUTPUT_DIR/version_comparison.md" << 'EOF'
# libfuse-fs 0.1.8 vs 0.1.9 版本对比

## API 变更

### Layer Trait

| 版本 | 方法名 | 签名 |
|------|--------|------|
| 0.1.8 | `do_getattr_helper` | `async fn do_getattr_helper(&self, inode: Inode, handle: Option<u64>) -> Result<(stat64, Duration)>` |
| 0.1.9 | `getattr_with_mapping` | `async fn getattr_with_mapping(&self, inode: Inode, handle: Option<u64>, mapping: bool) -> Result<(stat64, Duration)>` |

### OverlayFS Copy-up 调用

| 操作 | 0.1.8 版本 | 0.1.9 版本 |
|------|-----------|-----------|
| create_upper_dir | `do_getattr_helper(inode, None)` | `getattr_with_mapping(inode, None, false)` |
| copy_regfile_up | `do_getattr_helper(inode, None)` | `getattr_with_mapping(inode, None, false)` |
| copy_symlink_up | `do_getattr_helper(inode, None)` | `getattr_with_mapping(inode, None, false)` |

## 语义变更

- **0.1.8**: `do_getattr_helper` = "绕过 ID 映射，获取原始属性"
- **0.1.9**: `getattr_with_mapping(..., false)` = "mapping=false，获取未映射的原始属性"

两者功能相同，但 0.1.9 的 API 更清晰，支持可选的 ID 映射控制。

## 迁移指南

如果从 0.1.8 升级到 0.1.9：

1. 将 `do_getattr_helper` 重命名为 `getattr_with_mapping`
2. 添加 `mapping: bool` 参数
3. 对于只读层（如 Dicfuse），可以忽略 `mapping` 参数
4. 对于需要 ID 映射的层，根据 `mapping` 参数决定是否应用映射
EOF

echo ""
echo "=== 分析完成 ==="
echo ""
echo "分析结果保存在: $OUTPUT_DIR"
echo ""
echo "生成的文件:"
echo "  - layer_trait_analysis.md: Layer trait 方法签名分析"
echo "  - copy_regfile_up_analysis.md: Copy-up 方法详细分析"
echo "  - call_sites_analysis.md: 调用点分析"
echo "  - call_chain_analysis.md: 调用链分析"
echo "  - version_comparison.md: 版本对比"
echo ""
echo "查看分析结果:"
echo "  cat $OUTPUT_DIR/call_chain_analysis.md"

