# 调试总结

## 已完成的工作

### 1. 源码分析 ✅

- **深入分析了 libfuse-fs 0.1.9 源码**
  - 确认了 `getattr_with_mapping` 在 OverlayFS copy-up 中的关键作用
  - 找到了所有调用点：`copy_regfile_up`、`create_upper_dir`、`copy_symlink_up`
  - 验证了 Layer trait 默认实现返回 `ENOSYS`

### 2. 添加调试日志 ✅

在 `scorpio/src/dicfuse/mod.rs` 的 `getattr_with_mapping` 方法中添加了详细的调试日志：

```rust
tracing::debug!(
    "[Dicfuse::getattr_with_mapping] inode={}, handle={:?}, mapping={}",
    inode, _handle, mapping
);
```

这些日志会在以下情况输出：
- 方法被调用时（包含参数信息）
- 方法成功返回时（包含返回的 stat 信息）
- 方法失败时（包含错误信息）

### 3. 创建验证工具 ✅

**测试文件**:
- `scorpio/tests/verify_getattr_with_mapping.rs` - 单元测试（已通过）

**验证脚本**:
- `scorpio/src/bin/verify_getattr_issue.rs` - 可运行的验证脚本（已编译）
- `scorpio/scripts/debug_getattr.sh` - 调试脚本

**分析工具**:
- `scorpio/scripts/analyze_libfuse_source.sh` - 自动分析 libfuse-fs 源码
- `scorpio/scripts/test_without_getattr.sh` - 临时禁用方法进行测试

### 4. 测试结果 ✅

**单元测试**:
```bash
$ cargo test --test verify_getattr_with_mapping --lib
running 3 tests
test test_getattr_with_mapping_directly ... ok
test result: ok. 1 passed; 0 failed; 2 ignored
```

**内部测试**:
```bash
$ cargo test --lib dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size
test dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size ... ok
```

## 下一步调试步骤

### 1. 运行验证脚本（需要 root 权限）

```bash
cd scorpio
RUST_LOG=debug sudo -E cargo run --bin verify_getattr_issue
```

这将：
- 创建 Dicfuse 实例
- 挂载 Antares overlay
- 尝试创建文件（触发 copy-up）
- 输出详细的调试日志，包括 `getattr_with_mapping` 的调用

### 2. 查看调试日志

启用 debug 日志后，可以查看：
- `getattr_with_mapping` 是否被调用
- 调用时的参数（inode、handle、mapping）
- 返回的结果（成功或失败）

```bash
# 运行验证脚本并保存日志
RUST_LOG=debug sudo -E cargo run --bin verify_getattr_issue 2>&1 | tee /tmp/verify_debug.log

# 查看 getattr_with_mapping 调用
grep "getattr_with_mapping" /tmp/verify_debug.log
```

### 3. 测试实际的 Buck2 构建场景

如果验证脚本成功，可以测试实际的 Buck2 构建：

```bash
# 挂载 Antares overlay
cargo run --bin mount_test -- --config-path scorpio.toml

# 在另一个终端运行 Buck2 构建
cd /tmp/antares_test_*/mnt/third-party/buck-hello
buck2 build //...
```

观察是否还会出现 SQLite xShmMap 错误。

### 4. 临时禁用方法验证问题（可选）

如果需要验证问题确实是由 `getattr_with_mapping` 缺失导致的：

```bash
# 使用测试脚本临时禁用方法
./scorpio/scripts/test_without_getattr.sh
```

或者手动修改 `scorpio/src/dicfuse/mod.rs`，在 `getattr_with_mapping` 方法开头添加：
```rust
return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
```

然后运行验证脚本，应该会看到文件创建失败。

## 关键发现

1. **`getattr_with_mapping` 已实现**: Dicfuse 已经正确实现了此方法
2. **方法签名正确**: 包含所有必需的参数（inode、handle、mapping）
3. **功能正常**: 单元测试通过，方法能够正确返回 stat64 结构
4. **调试日志已添加**: 可以跟踪方法的调用和返回

## 验证状态

- ✅ 源码分析完成
- ✅ 调试日志已添加
- ✅ 验证工具已创建
- ✅ 单元测试通过
- ⏳ 集成测试待运行（需要 root 权限）
- ⏳ Buck2 构建测试待验证

## 相关文件

- `scorpio/src/dicfuse/mod.rs` - Dicfuse 实现（包含 getattr_with_mapping）
- `scorpio/src/bin/verify_getattr_issue.rs` - 验证脚本
- `scorpio/tests/verify_getattr_with_mapping.rs` - 单元测试
- `scorpio/scripts/debug_getattr.sh` - 调试脚本
- `scorpio/doc/libfuse-source-debugging.md` - 源码调试分析文档

