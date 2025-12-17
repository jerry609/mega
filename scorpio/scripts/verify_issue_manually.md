# 手动验证 getattr_with_mapping 问题

## 方法 1: 临时禁用 getattr_with_mapping 实现

### 步骤 1: 修改 `src/dicfuse/mod.rs`

找到 `getattr_with_mapping` 方法（大约在第 101 行），临时修改为：

```rust
async fn getattr_with_mapping(
    &self,
    inode: Inode,
    _handle: Option<u64>,
    _mapping: bool,
) -> std::io::Result<(libc::stat64, std::time::Duration)> {
    // 临时禁用：返回 ENOSYS 来验证问题
    return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
    
    // 原始实现被注释...
    // let item = self.store.get_inode(inode).await...
}
```

### 步骤 2: 编译并运行验证脚本

```bash
cd scorpio
cargo build --bin verify_getattr_issue
sudo cargo run --bin verify_getattr_issue
```

### 步骤 3: 预期结果

如果 `getattr_with_mapping` 被禁用，应该看到：
- 文件创建失败
- 错误信息：`Function not implemented` (ENOSYS)
- Copy-up 操作失败

### 步骤 4: 恢复原始实现

```bash
git checkout src/dicfuse/mod.rs
```

## 方法 2: 使用 git 分支对比

### 步骤 1: 创建测试分支

```bash
git checkout -b test/disable-getattr-with-mapping
```

### 步骤 2: 修改实现（同上）

### 步骤 3: 运行测试

```bash
cargo test --test verify_getattr_with_mapping
```

### 步骤 4: 对比结果

```bash
# 在禁用版本
git diff HEAD~1 src/dicfuse/mod.rs

# 切换回正常版本
git checkout feature/dicfuse-global-singleton
```

## 方法 3: 使用条件编译

在 `src/dicfuse/mod.rs` 中添加：

```rust
async fn getattr_with_mapping(
    &self,
    inode: Inode,
    _handle: Option<u64>,
    _mapping: bool,
) -> std::io::Result<(libc::stat64, std::time::Duration)> {
    #[cfg(feature = "disable-getattr-mapping")]
    {
        return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
    }
    
    // 正常实现...
}
```

然后使用：
```bash
cargo build --features disable-getattr-mapping --bin verify_getattr_issue
```

## 验证要点

1. **直接测试方法**：
   ```rust
   let result = dic.getattr_with_mapping(1, None, false).await;
   assert!(result.is_err() && result.unwrap_err().raw_os_error() == Some(libc::ENOSYS));
   ```

2. **测试 copy-up**：
   - 在挂载点上创建文件
   - 如果父目录在 lower layer，会触发 copy-up
   - Copy-up 需要调用 `getattr_with_mapping`

3. **测试 Buck2 场景**：
   - 挂载 Antares overlay
   - 运行 `buck2 build //...`
   - 观察 SQLite 初始化是否失败

## 预期行为对比

| 场景 | getattr_with_mapping 已实现 | getattr_with_mapping 未实现 |
|------|---------------------------|---------------------------|
| 直接调用方法 | 返回 Ok((stat, ttl)) | 返回 Err(ENOSYS) |
| 文件创建（无 copy-up） | 成功 | 成功（如果不需要 copy-up） |
| 文件创建（需要 copy-up） | 成功 | 失败（ENOSYS） |
| Buck2 构建 | 成功 | 失败（SQLite xShmMap 错误） |

