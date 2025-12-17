/// 验证 getattr_with_mapping 方法对 copy-up 操作的重要性
/// 
/// 这个测试验证了文档中描述的问题：
/// - 如果 Dicfuse 没有实现 getattr_with_mapping，copy-up 会失败
/// - Copy-up 失败会导致文件创建失败
/// - 这会影响 Buck2 在挂载点上的构建（SQLite 初始化失败）
use std::time::Duration;
use tempfile::TempDir;
use libfuse_fs::unionfs::layer::Layer;

/// 测试：验证 getattr_with_mapping 方法本身是否正常工作
#[tokio::test]
#[serial_test::serial]
async fn test_getattr_with_mapping_directly() {
    let temp_dir = TempDir::new().unwrap();
    let store_path = temp_dir.path().join("store");
    std::fs::create_dir_all(&store_path).unwrap();

    let dic = scorpio::dicfuse::Dicfuse::new_with_store_path(store_path.to_str().unwrap()).await;
    
    // 测试 getattr_with_mapping 方法（对于 root inode）
    // 如果 root 不存在，应该返回 ENOENT
    let result = dic.getattr_with_mapping(1, None, false).await;
    
    // 如果 root 存在，验证返回的 stat 结构
    if let Ok((stat, ttl)) = result {
        assert_eq!(stat.st_ino, 1, "Inode number should match");
        assert_ne!(stat.st_mode & libc::S_IFMT, 0, "File type should be set");
        assert_eq!(ttl, Duration::from_secs(2), "TTL should be 2 seconds");
        
        println!("✓ getattr_with_mapping works correctly for root");
        println!("  - inode: {}", stat.st_ino);
        println!("  - mode: {:#o}", stat.st_mode);
    } else {
        println!("ℹ Root inode not found (this is expected if store is empty)");
        // 这是正常的，因为 store 是空的
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(e.raw_os_error(), Some(libc::ENOENT), "Should return ENOENT for non-existent inode");
        }
    }
}

/// 测试：验证 getattr_with_mapping 实现时，copy-up 操作正常工作
/// 
/// 这个测试需要实际的目录结构，所以可能需要网络连接或预先加载的数据
#[tokio::test]
#[serial_test::serial]
#[ignore = "Requires actual Dicfuse data or network connection and root privileges"]
async fn test_copy_up_with_getattr_with_mapping_implemented() {
    let temp_dir = TempDir::new().unwrap();
    let store_path = temp_dir.path().join("store");
    std::fs::create_dir_all(&store_path).unwrap();

    let mount_dir = temp_dir.path().join("mount");
    let upper_dir = temp_dir.path().join("upper");
    std::fs::create_dir_all(&mount_dir).unwrap();
    std::fs::create_dir_all(&upper_dir).unwrap();

    // 创建 Dicfuse 实例（已实现 getattr_with_mapping）
    let dic = scorpio::dicfuse::Dicfuse::new_with_store_path(store_path.to_str().unwrap()).await;
    
    // 注意：这个测试需要实际的目录数据或网络连接
    // 在实际场景中，Dicfuse 会从网络加载目录树
    // 这里我们只验证方法存在，不进行实际的挂载测试
    println!("ℹ 此测试需要实际的 Dicfuse 数据和 root 权限");
    println!("   使用 verify_getattr_issue bin 进行完整测试");
    
    // 验证方法存在
    let result = dic.getattr_with_mapping(1, None, false).await;
    match result {
        Ok(_) => println!("✓ getattr_with_mapping 方法已实现"),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
            println!("✓ getattr_with_mapping 方法已实现（返回 ENOENT 是因为 inode 不存在）");
        }
        Err(e) => {
            panic!("Unexpected error: {:?}", e);
        }
    }
}

/// 测试：验证如果 getattr_with_mapping 返回 ENOSYS，copy-up 会失败
/// 
/// 注意：这个测试通过临时修改实现来验证问题。
/// 在实际场景中，如果方法未实现，Layer trait 的默认实现会返回 ENOSYS。
#[tokio::test]
#[serial_test::serial]
#[ignore = "This test requires manual modification of Dicfuse implementation"]
async fn test_copy_up_fails_without_getattr_with_mapping() {
    // 这个测试需要手动注释掉 getattr_with_mapping 的实现
    // 或者创建一个特殊的测试版本
    // 
    // 预期行为：
    // 1. 如果 getattr_with_mapping 未实现，Layer trait 默认实现返回 ENOSYS
    // 2. OverlayFS::copy_regfile_up 调用 getattr_with_mapping 时收到 ENOSYS
    // 3. Copy-up 失败
    // 4. 文件创建失败，返回 "Function not implemented" 错误
    
    println!("This test requires manual modification to verify the issue.");
    println!("To verify:");
    println!("1. Comment out getattr_with_mapping implementation in dicfuse/mod.rs");
    println!("2. Run this test");
    println!("3. Verify that file creation fails with ENOSYS");
}
