# libfuse-fs getattr_with_mapping 验证总结

## 已完成的工作

### 1. 源码深度分析

通过深入分析 libfuse-fs 0.1.9 源码，我们确认了以下关键点：

- **Layer trait 默认实现**: 返回 `ENOSYS` (Function not implemented)
- **OverlayFS copy-up 依赖**: `copy_regfile_up`、`create_upper_dir`、`copy_symlink_up` 都依赖 `getattr_with_mapping`
- **错误传播路径**: 未实现 → ENOSYS → copy-up 失败 → 文件创建失败 → Buck2 SQLite 错误

### 2. 创建的文件

1. **验证脚本**:
   - `scorpio/src/bin/verify_getattr_issue.rs`: 可运行的验证脚本
   - `scorpio/tests/verify_getattr_with_mapping.rs`: 单元测试

2. **分析脚本**:
   - `scorpio/scripts/analyze_libfuse_source.sh`: 自动分析 libfuse-fs 源码
   - `scorpio/scripts/test_without_getattr.sh`: 临时禁用方法进行测试
   - `scorpio/scripts/verify_issue_manually.md`: 手动验证指南

3. **文档**:
   - `scorpio/doc/libfuse-source-debugging.md`: 源码调试分析文档
   - `scorpio/doc/libfuse-source-analysis/`: 自动生成的分析结果

### 3. 关键发现

#### 3.1 源码位置

| 组件 | 文件位置 | 行号 | 说明 |
|------|---------|------|------|
| Layer trait 默认实现 | `libfuse-fs-0.1.9/src/unionfs/layer.rs` | 223-230 | 返回 ENOSYS |
| copy_regfile_up | `libfuse-fs-0.1.9/src/unionfs/mod.rs` | 2176-2200 | 调用 getattr_with_mapping |
| create_upper_dir | `libfuse-fs-0.1.9/src/unionfs/mod.rs` | 730-743 | 调用 getattr_with_mapping |
| copy_symlink_up | `libfuse-fs-0.1.9/src/unionfs/mod.rs` | 2077-2106 | 调用 getattr_with_mapping |

#### 3.2 调用链

```
用户操作 (touch file.txt)
  └── FUSE 内核
      └── OverlayFS::create
          └── OverlayFS::copy_node_up
              └── OverlayFS::copy_regfile_up
                  └── lower_layer.getattr_with_mapping(..., false)  ← 关键调用
                      └── 如果未实现 → ENOSYS
                          └── copy-up 失败
                              └── 文件创建失败
```

### 4. 验证方法

#### 方法 1: 直接测试方法

```rust
use libfuse_fs::unionfs::layer::Layer;

let dic = Dicfuse::new().await;
let result = dic.getattr_with_mapping(1, None, false).await;

match result {
    Ok(_) => println!("✓ 方法已实现"),
    Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
        println!("✗ 方法未实现 (ENOSYS)");
    }
    Err(e) => println!("? 其他错误: {:?}", e),
}
```

#### 方法 2: 测试 copy-up

```bash
# 挂载 Antares overlay
cargo run --bin mount_test

# 在挂载点上创建文件（触发 copy-up）
touch /tmp/antares_test_*/mnt/test_file.txt

# 如果出现 "Function not implemented"，说明 getattr_with_mapping 未实现
```

#### 方法 3: 运行验证脚本

```bash
cd scorpio
cargo build --bin verify_getattr_issue
sudo cargo run --bin verify_getattr_issue
```

#### 方法 4: 源码分析

```bash
cd scorpio
./scripts/analyze_libfuse_source.sh
cat doc/libfuse-source-analysis/call_chain_analysis.md
```

### 5. 当前状态

✅ **Dicfuse 已实现 `getattr_with_mapping`**
- 位置: `scorpio/src/dicfuse/mod.rs:101-166`
- 功能: 从 StorageItem 构造 stat64 结构
- 测试: `scorpio/src/dicfuse/mod.rs:365-389`

✅ **libfuse-fs 版本**: 0.1.9
- 位置: `scorpio/Cargo.toml:47`

✅ **验证工具已创建**
- 验证脚本、测试、分析工具都已就绪

### 6. 下一步

1. **运行验证脚本**: 确认当前实现正常工作
2. **测试 Buck2 构建**: 验证 SQLite 初始化不再失败
3. **添加更多测试**: 覆盖更多 copy-up 场景

### 7. 相关文档

- `scorpio/doc/libfuse-fs-version-deep-dive.md`: 版本对比分析
- `scorpio/doc/libfuse-source-debugging.md`: 源码调试分析
- `scorpio/doc/buck2_fuse_build_debugging.md`: Buck2 构建问题调试

### 8. 分支信息

当前分支: `debug/verify-libfuse-fs-issue`

创建时间: 用于验证和调试 libfuse-fs getattr_with_mapping 问题

