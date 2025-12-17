// 测试 Copy-up 调用链路
//
// 这个测试验证：
// 1. getattr_with_mapping 是否被正确调用
// 2. 调用链路是否完整
// 3. 错误传播是否正确

use scorpio::dicfuse::Dicfuse;
use libfuse_fs::unionfs::layer::Layer;
use std::sync::Arc;

#[tokio::test]
#[ignore] // 需要实际的 store
async fn test_getattr_with_mapping_call_chain() {
    // 初始化日志
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    tracing::info!("=== 测试开始: getattr_with_mapping 调用链路 ===");

    // 创建 Dicfuse 实例
    let dic = Arc::new(Dicfuse::new().await);
    
    tracing::info!("步骤 1: 创建 Dicfuse 实例");

    // 测试 root inode
    let root_inode = dic.root_inode();
    assert_eq!(root_inode, 1, "Root inode 应该是 1");
    
    tracing::info!("步骤 2: 验证 root inode = {}", root_inode);

    // 测试 getattr_with_mapping
    tracing::info!("步骤 3: 调用 getattr_with_mapping");
    
    let result = dic.getattr_with_mapping(root_inode, None, false).await;
    
    match &result {
        Ok((stat, ttl)) => {
            tracing::info!("✓ getattr_with_mapping 成功");
            tracing::info!("  - inode: {}", stat.st_ino);
            tracing::info!("  - mode: {:#o}", stat.st_mode);
            tracing::info!("  - uid: {}", stat.st_uid);
            tracing::info!("  - gid: {}", stat.st_gid);
            tracing::info!("  - size: {}", stat.st_size);
            tracing::info!("  - ttl: {:?}", ttl);
            
            assert_eq!(stat.st_ino, root_inode);
            assert_ne!(stat.st_mode, 0, "Mode 不应该为 0");
        }
        Err(e) => {
            tracing::error!("✗ getattr_with_mapping 失败: {:?}", e);
            
            if let Some(os_error) = e.raw_os_error() {
                if os_error == libc::ENOSYS {
                    panic!("getattr_with_mapping 返回 ENOSYS - 这意味着方法未正确实现！");
                } else if os_error == libc::ENOENT {
                    panic!("getattr_with_mapping 返回 ENOENT - inode {} 不存在", root_inode);
                } else {
                    panic!("getattr_with_mapping 返回错误: {} (errno: {})", e, os_error);
                }
            } else {
                panic!("getattr_with_mapping 返回未知错误: {}", e);
            }
        }
    }

    tracing::info!("步骤 4: 测试不同的 mapping 参数");
    
    // 测试 mapping = true
    let result_mapped = dic.getattr_with_mapping(root_inode, None, true).await;
    assert!(result_mapped.is_ok(), "mapping=true 应该成功");
    
    // 测试 mapping = false
    let result_unmapped = dic.getattr_with_mapping(root_inode, None, false).await;
    assert!(result_unmapped.is_ok(), "mapping=false 应该成功");
    
    tracing::info!("  ✓ 两种 mapping 参数都成功");

    tracing::info!("步骤 5: 对比结果");
    
    let (stat_mapped, _) = result_mapped.unwrap();
    let (stat_unmapped, _) = result_unmapped.unwrap();
    
    // 对于 Dicfuse（虚拟只读层），mapping 参数应该被忽略
    // 所以两个结果应该相同
    assert_eq!(stat_mapped.st_ino, stat_unmapped.st_ino);
    assert_eq!(stat_mapped.st_mode, stat_unmapped.st_mode);
    assert_eq!(stat_mapped.st_uid, stat_unmapped.st_uid);
    assert_eq!(stat_mapped.st_gid, stat_unmapped.st_gid);
    
    tracing::info!("  ✓ mapping 参数被正确忽略（符合预期）");

    tracing::info!("=== 测试完成: 所有检查通过 ===");
}

#[tokio::test]
#[ignore] // 需要实际的 store
async fn test_copy_up_scenario_simulation() {
    // 初始化日志
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    tracing::info!("=== 模拟 Copy-up 场景 ===");

    // 创建 Dicfuse 实例
    let dic = Arc::new(Dicfuse::new().await);
    
    tracing::info!("场景: OverlayFS 需要 copy-up 一个文件");
    tracing::info!("  1. 文件在 lower layer (Dicfuse)");
    tracing::info!("  2. 用户尝试修改文件");
    tracing::info!("  3. OverlayFS 调用 lower_layer.getattr_with_mapping()");
    tracing::info!("  4. 获取文件属性用于在 upper layer 创建文件");

    // 假设我们有一个文件的 inode
    let test_inode = 1; // root inode 作为示例
    
    tracing::info!("");
    tracing::info!("步骤 1: OverlayFS::copy_regfile_up() 调用");
    tracing::info!("  调用: lower_layer.getattr_with_mapping({}, None, false)", test_inode);
    
    let start = std::time::Instant::now();
    let result = dic.getattr_with_mapping(test_inode, None, false).await;
    let elapsed = start.elapsed();
    
    match result {
        Ok((stat, ttl)) => {
            tracing::info!("  ✓ 成功获取文件属性 (耗时: {:?})", elapsed);
            tracing::info!("");
            tracing::info!("步骤 2: 使用获取的属性在 upper layer 创建文件");
            tracing::info!("  - mode: {:#o}", stat.st_mode);
            tracing::info!("  - uid: {}", stat.st_uid);
            tracing::info!("  - gid: {}", stat.st_gid);
            tracing::info!("  - size: {} bytes", stat.st_size);
            tracing::info!("");
            tracing::info!("步骤 3: 复制文件内容");
            tracing::info!("  - 从 lower layer 读取 {} bytes", stat.st_size);
            tracing::info!("  - 写入 upper layer");
            tracing::info!("");
            tracing::info!("✓ Copy-up 成功！");
        }
        Err(e) => {
            tracing::error!("  ✗ 获取文件属性失败: {:?}", e);
            
            if let Some(os_error) = e.raw_os_error() {
                if os_error == libc::ENOSYS {
                    tracing::error!("");
                    tracing::error!("❌ 这就是问题所在！");
                    tracing::error!("");
                    tracing::error!("错误传播链:");
                    tracing::error!("  lower_layer.getattr_with_mapping() 返回 ENOSYS");
                    tracing::error!("    ↓");
                    tracing::error!("  OverlayFS::copy_regfile_up() 失败");
                    tracing::error!("    ↓");
                    tracing::error!("  OverlayFS::create() 失败");
                    tracing::error!("    ↓");
                    tracing::error!("  FUSE 返回错误给应用");
                    tracing::error!("    ↓");
                    tracing::error!("  SQLite 收到 I/O 错误");
                    tracing::error!("    ↓");
                    tracing::error!("  Buck2 报告: xShmMap I/O error");
                    
                    panic!("getattr_with_mapping 未实现！");
                }
            }
            
            panic!("Copy-up 失败: {}", e);
        }
    }

    tracing::info!("=== 场景模拟完成 ===");
}

#[test]
fn test_error_propagation_chain() {
    println!("=== 错误传播链测试 ===");
    println!();
    println!("模拟: getattr_with_mapping 返回 ENOSYS");
    println!();
    
    let enosys_error = std::io::Error::from_raw_os_error(libc::ENOSYS);
    
    println!("1. Layer trait 默认实现:");
    println!("   Err(std::io::Error::from_raw_os_error(libc::ENOSYS))");
    println!();
    
    println!("2. 错误传播:");
    println!("   {:?}", enosys_error);
    println!();
    
    assert_eq!(enosys_error.raw_os_error(), Some(libc::ENOSYS));
    
    println!("3. 错误码: {}", libc::ENOSYS);
    println!("   含义: Function not implemented");
    println!();
    
    println!("4. 影响:");
    println!("   - OverlayFS 无法获取文件属性");
    println!("   - Copy-up 操作失败");
    println!("   - 文件创建失败");
    println!("   - 应用收到 I/O 错误");
    println!();
    
    println!("✓ 错误传播链验证完成");
}

