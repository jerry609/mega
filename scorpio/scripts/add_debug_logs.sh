#!/bin/bash
# 在关键调用点添加调试日志
#
# 使用方法：
#   ./scripts/add_debug_logs.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORPIO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$SCORPIO_DIR"

echo "========================================="
echo "在关键调用点添加调试日志"
echo "========================================="
echo ""

MOD_FILE="src/dicfuse/mod.rs"

echo "1. 备份原文件..."
cp "$MOD_FILE" "${MOD_FILE}.backup"
echo "   ✓ 备份完成: ${MOD_FILE}.backup"
echo ""

echo "2. 检查当前实现..."
echo ""

if grep -q "async fn getattr_with_mapping" "$MOD_FILE"; then
    echo "   ✓ 发现 getattr_with_mapping 实现"
    
    # 检查是否已经有调试日志
    if grep -q "\[Dicfuse::getattr_with_mapping\] ENTER" "$MOD_FILE"; then
        echo "   ⚠️ 调试日志已存在"
    else
        echo "   添加详细的调试日志..."
        
        # 在方法开始处添加 ENTER 日志
        # 在方法各个关键点添加详细日志
        
        cat > /tmp/add_debug_logs.patch << 'EOF'
在 getattr_with_mapping 方法中添加以下日志点：

1. 方法入口：
   tracing::debug!("[Dicfuse::getattr_with_mapping] ENTER: inode={}, handle={:?}, mapping={}", inode, _handle, mapping);

2. 获取 inode 后：
   tracing::debug!("[Dicfuse::getattr_with_mapping] Got inode, is_dir={}", item.is_dir());

3. 构造 stat64 后：
   tracing::debug!("[Dicfuse::getattr_with_mapping] Constructed stat64: mode={:#o}, size={}, uid={}, gid={}", 
       stat.st_mode, stat.st_size, stat.st_uid, stat.st_gid);

4. 返回前：
   tracing::debug!("[Dicfuse::getattr_with_mapping] EXIT: SUCCESS");

5. 错误处理：
   tracing::error!("[Dicfuse::getattr_with_mapping] ERROR: {:?}", e);
EOF
        
        echo ""
        echo "   建议的调试日志点:"
        cat /tmp/add_debug_logs.patch | sed 's/^/   /'
    fi
else
    echo "   ✗ 未发现 getattr_with_mapping 实现"
    echo ""
    echo "   这意味着:"
    echo "   - 代码可能使用 0.1.8 的 do_getattr_helper"
    echo "   - 或者完全没有实现"
fi

echo ""
echo "========================================="
echo "3. 查看当前的日志实现..."
echo ""

echo "现有的 tracing::debug 日志:"
grep -n "tracing::debug.*getattr" "$MOD_FILE" | sed 's/^/   /' || echo "   (未发现)"

echo ""
echo "现有的 tracing::warn 日志:"
grep -n "tracing::warn.*getattr" "$MOD_FILE" | sed 's/^/   /' || echo "   (未发现)"

echo ""
echo "========================================="
echo "4. 建议的日志策略..."
echo ""

cat << 'EOF'
为了完整追踪调用链路，建议添加以下日志点：

A. 在 Dicfuse::getattr_with_mapping 中：
   
   1. 入口日志（ENTER）:
      tracing::info!("🔵 [Dicfuse::getattr_with_mapping] ENTER");
      tracing::debug!("   ├─ inode: {}", inode);
      tracing::debug!("   ├─ handle: {:?}", _handle);
      tracing::debug!("   └─ mapping: {}", mapping);

   2. 关键步骤日志：
      tracing::debug!("🔵 [Dicfuse::getattr_with_mapping] Calling store.get_inode({})", inode);
      tracing::debug!("🔵 [Dicfuse::getattr_with_mapping] Got item, type: {:?}", item_type);
      tracing::debug!("🔵 [Dicfuse::getattr_with_mapping] Constructing stat64...");

   3. 成功返回日志：
      tracing::info!("🟢 [Dicfuse::getattr_with_mapping] SUCCESS");
      tracing::debug!("   ├─ inode: {}", stat.st_ino);
      tracing::debug!("   ├─ mode: {:#o}", stat.st_mode);
      tracing::debug!("   ├─ size: {}", stat.st_size);
      tracing::debug!("   ├─ uid: {}", stat.st_uid);
      tracing::debug!("   └─ gid: {}", stat.st_gid);

   4. 错误日志（如果失败）：
      tracing::error!("🔴 [Dicfuse::getattr_with_mapping] ERROR: {:?}", e);

B. 如果测试 0.1.8 版本，在默认实现中添加：
   
   tracing::error!("🔴 [Layer::do_getattr_helper] DEFAULT IMPL CALLED - RETURNING ENOSYS");
   tracing::error!("   This means Dicfuse did not implement do_getattr_helper!");

C. 运行时使用的环境变量：
   
   export RUST_LOG="scorpio=trace,libfuse_fs=debug"
   # trace 级别可以看到所有细节

使用这些日志后，可以清楚地看到：
- 方法是否被调用
- 调用的参数
- 执行的每一步
- 返回的结果
- 如果失败，失败的原因
EOF

echo ""
echo "========================================="
echo "5. 快速添加日志的方法..."
echo ""

cat << 'EOF'
手动编辑 src/dicfuse/mod.rs，在 getattr_with_mapping 方法中添加：

async fn getattr_with_mapping(
    &self,
    inode: Inode,
    _handle: Option<u64>,
    mapping: bool,
) -> std::io::Result<(libc::stat64, std::time::Duration)> {
    // 🔵 入口日志
    tracing::info!("🔵 [ENTER] Dicfuse::getattr_with_mapping");
    tracing::debug!("   inode={}, handle={:?}, mapping={}", inode, _handle, mapping);
    
    // 🔵 获取 inode
    tracing::debug!("🔵 [STEP 1] Calling store.get_inode({})", inode);
    let item = self
        .store
        .get_inode(inode)
        .await
        .map_err(|e| {
            // 🔴 错误日志
            tracing::error!("🔴 [ERROR] Failed to get inode {}: {:?}", inode, e);
            std::io::Error::from_raw_os_error(libc::ENOENT)
        })?;
    tracing::debug!("🔵 [STEP 1] Got item successfully");
    
    // ... 其他代码 ...
    
    // 🟢 成功返回日志
    tracing::info!("🟢 [EXIT] Dicfuse::getattr_with_mapping SUCCESS");
    tracing::debug!("   mode={:#o}, size={}, uid={}, gid={}", 
        stat.st_mode, stat.st_size, stat.st_uid, stat.st_gid);
    
    Ok((stat, std::time::Duration::from_secs(2)))
}

然后运行：
  RUST_LOG=scorpio=trace cargo test --test test_copy_up_chain -- --nocapture
EOF

echo ""
echo "========================================="
echo ""
echo "注意: 原文件已备份为 ${MOD_FILE}.backup"
echo "如需恢复: mv ${MOD_FILE}.backup $MOD_FILE"

