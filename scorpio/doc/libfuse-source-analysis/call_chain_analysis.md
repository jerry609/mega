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
