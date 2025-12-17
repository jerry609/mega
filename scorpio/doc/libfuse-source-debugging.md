# libfuse-fs 源码深度调试分析

## 概述

本文档通过深入分析 libfuse-fs 0.1.9 的源码，验证 `getattr_with_mapping` 方法对 OverlayFS copy-up 操作的重要性。

## 关键发现

### 1. Layer Trait 默认实现

**位置**: `libfuse-fs-0.1.9/src/unionfs/layer.rs:223-230`

```rust
/// Retrieve metadata with optional ID mapping control.
///
/// - `mapping: true`: Returns attributes as seen inside the container (mapped).
/// - `mapping: false`: Returns raw attributes on the host filesystem (unmapped).
async fn getattr_with_mapping(
    &self,
    _inode: Inode,
    _handle: Option<u64>,
    _mapping: bool,
) -> std::io::Result<(libc::stat64, Duration)> {
    Err(std::io::Error::from_raw_os_error(libc::ENOSYS))  // ← 默认返回 ENOSYS
}
```

**关键点**:
- 如果 Layer 实现没有覆盖此方法，默认返回 `ENOSYS` (Function not implemented)
- 这会导致所有依赖此方法的操作失败

### 2. OverlayFS::copy_regfile_up 方法

**位置**: `libfuse-fs-0.1.9/src/unionfs/mod.rs:2176-2200`

```rust
async fn copy_regfile_up(
    &self,
    ctx: Request,
    node: Arc<OverlayInode>,
) -> Result<Arc<OverlayInode>> {
    if node.in_upper_layer().await {
        return Ok(node);
    }

    let parent_node = if let Some(ref n) = node.parent.lock().await.upgrade() {
        Arc::clone(n)
    } else {
        return Err(Error::other("no parent?"));
    };

    // To preserve original ownership, we must get the raw, unmapped host attributes.
    // We achieve this by calling `getattr_with_mapping` with `mapping: false`.
    // This is safe and does not affect other functionalities because `getattr_with_mapping`
    // and the standard `stat64()` call both rely on the same underlying `stat` system call;
    // they only differ in whether the resulting `uid` and `gid` are mapped.
    let (lower_layer, _, lower_inode) = node.first_layer_inode().await;
    let re = lower_layer
        .getattr_with_mapping(lower_inode, None, false)  // ← 关键调用点
        .await?;  // ← 如果返回 ENOSYS，这里会失败
    let st = ReplyAttr {
        ttl: re.1,
        attr: convert_stat64_to_file_attr(re.0),
    };
    // ... 后续使用 st 创建文件 ...
}
```

**关键点**:
1. **必须调用**: `copy_regfile_up` 必须调用 `getattr_with_mapping` 来获取文件的原始属性
2. **错误传播**: 如果 `getattr_with_mapping` 返回 `ENOSYS`，整个 copy-up 操作会失败
3. **用途**: 获取文件的 mode、UID、GID、size 等属性，用于在 upper layer 创建文件

### 3. OverlayFS::create_upper_dir 方法

**位置**: `libfuse-fs-0.1.9/src/unionfs/mod.rs:730-743`

```rust
async fn create_upper_dir(
    self: Arc<Self>,
    ctx: Request,
    mode_umask: Option<(u32, u32)>,
) -> Result<()> {
    // To preserve original ownership, we must get the raw, unmapped host attributes.
    // We achieve this by calling `getattr_with_mapping` with `mapping: false`.
    let (self_layer, _, self_inode) = self.first_layer_inode().await;
    let re = self_layer
        .getattr_with_mapping(self_inode, None, false)  // ← 关键调用点
        .await?;  // ← 如果返回 ENOSYS，这里会失败
    let st = ReplyAttr {
        ttl: re.1,
        attr: convert_stat64_to_file_attr(re.0),
    };
    // ... 后续使用 st 创建目录 ...
}
```

**关键点**:
- 在创建目录的 copy-up 操作中，也需要调用 `getattr_with_mapping`
- 用于获取目录的原始属性（mode、UID、GID）

### 4. 调用链分析

#### 4.1 文件创建场景

```
用户操作: touch /mnt/path/to/file.txt
  │
  ▼
FUSE 内核: FUSE_CREATE 请求
  │
  ▼
OverlayFS::create (async_io.rs:150)
  │
  ├── 检查 upper layer 是否存在文件
  │   └── 不存在
  │
  ├── 检查 lower layer 是否存在文件
  │   └── 不存在（新文件）
  │
  └── 检查父目录是否在 upper layer
      │
      ├── 如果不在 upper layer
      │   └── OverlayFS::copy_node_up (mod.rs:2314)
      │       │
      │       └── 对于目录: OverlayFS::create_upper_dir (mod.rs:730)
      │           │
      │           └── lower_layer.getattr_with_mapping(..., false)  ← 调用点 1
      │               │
      │               └── 如果未实现 → 返回 ENOSYS
      │                   │
      │                   └── create_upper_dir 失败
      │                       │
      │                       └── copy_node_up 失败
      │                           │
      │                           └── OverlayFS::create 失败
      │                               │
      │                               └── 文件创建失败
```

#### 4.2 文件修改场景（copy-up）

```
用户操作: echo "content" > /mnt/path/to/existing_file.txt
  │
  ▼
FUSE 内核: FUSE_WRITE 请求
  │
  ▼
OverlayFS::write (async_io.rs)
  │
  ├── 检查文件是否在 upper layer
  │   └── 不在 upper layer（文件在 lower layer）
  │
  └── OverlayFS::copy_node_up (mod.rs:2314)
      │
      └── 对于普通文件: OverlayFS::copy_regfile_up (mod.rs:2176)
          │
          └── lower_layer.getattr_with_mapping(..., false)  ← 调用点 2
              │
              └── 如果未实现 → 返回 ENOSYS
                  │
                  └── copy_regfile_up 失败
                      │
                      └── copy_node_up 失败
                          │
                          └── OverlayFS::write 失败
                              │
                              └── 文件写入失败
```

### 5. 错误传播路径

#### 5.1 直接错误（ENOSYS）

如果 `getattr_with_mapping` 未实现：

```
Dicfuse::getattr_with_mapping (未实现)
  │
  └── Layer trait 默认实现
      │
      └── 返回 Err(ENOSYS)
          │
          ▼
OverlayFS::copy_regfile_up 收到 ENOSYS
          │
          ▼
? 操作符传播错误
          │
          ▼
copy_regfile_up 返回 Err(ENOSYS)
          │
          ▼
OverlayFS::copy_node_up 传播错误
          │
          ▼
OverlayFS::create 返回错误
          │
          ▼
FUSE 返回错误给内核
          │
          ▼
系统调用 creat() 返回 -1, errno = ENOSYS
          │
          ▼
用户看到: "Function not implemented"
```

#### 5.2 Buck2 场景（SQLite xShmMap 错误）

```
Buck2 初始化
  │
  └── SQLite 尝试创建 .buck2/daemon_state.db
      │
      └── SQLite 打开 WAL 模式
          │
          └── 需要创建 .shm 共享内存文件
              │
              └── 系统调用: creat("/mnt/.../.buck2/daemon_state.db-shm", ...)
                  │
                  ▼
FUSE: OverlayFS::create
                  │
                  └── 触发 copy-up（父目录在 lower layer）
                      │
                      └── OverlayFS::copy_node_up
                          │
                          └── OverlayFS::create_upper_dir
                              │
                              └── lower_layer.getattr_with_mapping(..., false)
                                  │
                                  └── 返回 ENOSYS
                                      │
                                      ▼
copy-up 失败
                                      │
                                      ▼
文件创建失败
                                      │
                                      ▼
SQLite 收到 I/O 错误
                                      │
                                      ▼
SQLite 报告: "Error code 5386: I/O error within the xShmMap method"
```

**为什么是 xShmMap 错误而不是 ENOSYS？**

1. **错误转换**: OverlayFS 可能将 `ENOSYS` 转换为其他错误类型
2. **SQLite 上下文**: SQLite 在创建 `.shm` 文件时失败，根据上下文报告为 xShmMap 错误
3. **错误信息误导性**: 表面的 SQLite 错误掩盖了真正的 FUSE Layer 实现问题

### 6. 源码验证

#### 6.1 验证方法存在性

```rust
use libfuse_fs::unionfs::layer::Layer;

// 测试 getattr_with_mapping 是否实现
let dic = Dicfuse::new().await;
let result = dic.getattr_with_mapping(1, None, false).await;

match result {
    Ok((stat, ttl)) => {
        println!("✓ getattr_with_mapping 已实现");
        println!("  - inode: {}", stat.st_ino);
        println!("  - mode: {:#o}", stat.st_mode);
    }
    Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
        println!("✗ getattr_with_mapping 未实现 (ENOSYS)");
    }
    Err(e) => {
        println!("? 其他错误: {:?}", e);
    }
}
```

#### 6.2 验证 copy-up 操作

```rust
// 在挂载点上创建文件，触发 copy-up
let test_file = mount_dir.join("test_file.txt");
match std::fs::write(&test_file, b"test") {
    Ok(_) => println!("✓ 文件创建成功，copy-up 正常"),
    Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
        println!("✗ 文件创建失败，copy-up 失败 (ENOSYS)");
        println!("  这可能是因为 getattr_with_mapping 未实现");
    }
    Err(e) => {
        println!("? 其他错误: {:?}", e);
    }
}
```

### 7. 关键代码位置总结

| 组件 | 文件位置 | 行号 | 说明 |
|------|---------|------|------|
| **Layer trait 默认实现** | `libfuse-fs-0.1.9/src/unionfs/layer.rs` | 223-230 | 返回 ENOSYS |
| **copy_regfile_up** | `libfuse-fs-0.1.9/src/unionfs/mod.rs` | 2176-2200 | 调用 getattr_with_mapping |
| **create_upper_dir** | `libfuse-fs-0.1.9/src/unionfs/mod.rs` | 730-743 | 调用 getattr_with_mapping |
| **copy_symlink_up** | `libfuse-fs-0.1.9/src/unionfs/mod.rs` | 2077-2106 | 调用 getattr_with_mapping |
| **Dicfuse 实现** | `scorpio/src/dicfuse/mod.rs` | 101-166 | 必须实现 getattr_with_mapping |

### 8. 验证结论

通过深入分析 libfuse-fs 0.1.9 源码，我们确认：

1. **`getattr_with_mapping` 是必需的**: OverlayFS 的 copy-up 操作强依赖此方法
2. **默认实现返回 ENOSYS**: 如果 Layer 实现没有覆盖此方法，默认返回 `ENOSYS`
3. **Copy-up 会失败**: 如果方法未实现，所有需要 copy-up 的操作都会失败
4. **错误传播**: Copy-up 失败会导致文件创建/修改失败，进而影响 Buck2 构建

**解决方案**: 在 Dicfuse 中实现 `getattr_with_mapping` 方法（已完成）。

### 9. 调试建议

1. **添加日志**: 在 `getattr_with_mapping` 实现中添加 trace 日志
2. **错误处理**: 检查错误类型，区分 `ENOSYS` 和其他错误
3. **测试覆盖**: 创建单元测试验证方法实现
4. **集成测试**: 测试实际的 copy-up 场景

### 10. 相关文件

- `scorpio/src/dicfuse/mod.rs`: Dicfuse 的 `getattr_with_mapping` 实现
- `scorpio/tests/verify_getattr_with_mapping.rs`: 验证测试
- `scorpio/src/bin/verify_getattr_issue.rs`: 验证脚本
- `scorpio/scripts/analyze_libfuse_source.sh`: 源码分析脚本

