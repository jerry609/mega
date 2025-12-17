# 验证 Buck2 SQLite xShmMap 错误的根本原因

## 问题描述

Buck2 在 Antares/Dicfuse 挂载点上构建时，可能出现以下错误：
```
Error code 5386: I/O error within the xShmMap method
```

根据源码分析，这个错误的根本原因是：**Dicfuse 未实现 `getattr_with_mapping` 方法，导致 OverlayFS copy-up 失败**。

## 验证步骤

### 步骤 1: 检查 getattr_with_mapping 是否已实现

#### 方法 1: 查看源码

```bash
cd scorpio
grep -A 5 "async fn getattr_with_mapping" src/dicfuse/mod.rs
```

**预期输出**（已实现）:
```rust
async fn getattr_with_mapping(
    &self,
    inode: Inode,
    _handle: Option<u64>,
    _mapping: bool,
) -> std::io::Result<(libc::stat64, std::time::Duration)> {
    // 实现代码...
}
```

**如果未实现**: 只会看到 Layer trait 的默认实现，或者方法不存在。

#### 方法 2: 运行单元测试

```bash
cd scorpio
cargo test --lib dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size
```

**预期结果**（已实现）:
```
test dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size ... ok
```

**如果未实现**: 测试会失败或编译错误。

#### 方法 3: 直接调用方法测试

创建测试脚本 `test_getattr.rs`:

```rust
use libfuse_fs::unionfs::layer::Layer;

#[tokio::main]
async fn main() {
    let dic = scorpio::dicfuse::Dicfuse::new_with_store_path("/tmp/test_store").await;
    let result = dic.getattr_with_mapping(1, None, false).await;
    
    match result {
        Ok(_) => println!("✓ getattr_with_mapping 已实现"),
        Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
            println!("✗ getattr_with_mapping 未实现 (ENOSYS)");
            println!("  这就是导致 Buck2 SQLite xShmMap 错误的根本原因！");
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
            println!("✓ getattr_with_mapping 已实现（返回 ENOENT 是因为 inode 不存在）");
        }
        Err(e) => println!("? 其他错误: {:?}", e),
    }
}
```

运行:
```bash
cd scorpio
cargo run --bin test_getattr  # 需要先添加到 Cargo.toml
```

### 步骤 2: 验证 copy-up 操作是否失败

#### 方法 1: 在挂载点上创建文件

```bash
# 1. 挂载 Antares overlay
cd scorpio
cargo run --bin mount_test -- --config-path scorpio.toml

# 2. 在另一个终端，尝试在挂载点上创建文件
cd /tmp/antares_test_*/mnt/third-party/buck-hello
touch test_file.txt
```

**如果 getattr_with_mapping 未实现**:
```
touch: cannot touch 'test_file.txt': Function not implemented
```

**如果已实现**:
```
# 文件创建成功，无错误
```

#### 方法 2: 使用验证脚本

```bash
cd scorpio
sudo ./scripts/run_verification.sh
```

查看输出中的文件创建结果。

### 步骤 3: 复现 Buck2 SQLite xShmMap 错误

#### 如果 getattr_with_mapping 未实现，可以复现错误：

```bash
# 1. 挂载 Antares overlay
cd scorpio
cargo run --bin mount_test -- --config-path scorpio.toml

# 2. 在挂载点上运行 Buck2
cd /tmp/antares_test_*/mnt/third-party/buck-hello
buck2 build //...
```

**预期错误**（如果 getattr_with_mapping 未实现）:
```
Error code 5386: I/O error within the xShmMap method
```

#### 临时禁用方法来验证问题

如果需要验证问题确实是由 `getattr_with_mapping` 缺失导致的：

1. **备份当前实现**:
```bash
cd scorpio
cp src/dicfuse/mod.rs src/dicfuse/mod.rs.backup
```

2. **临时禁用方法**:
编辑 `src/dicfuse/mod.rs`，在 `getattr_with_mapping` 方法开头添加：
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
}
```

3. **重新编译并测试**:
```bash
cd scorpio
cargo build
cargo run --bin mount_test -- --config-path scorpio.toml

# 在另一个终端
cd /tmp/antares_test_*/mnt/third-party/buck-hello
buck2 build //...
```

4. **应该看到 SQLite xShmMap 错误**

5. **恢复实现**:
```bash
cd scorpio
cp src/dicfuse/mod.rs.backup src/dicfuse/mod.rs
cargo build
```

### 步骤 4: 验证问题已解决

#### 方法 1: 运行完整验证

```bash
cd scorpio
sudo ./scripts/run_verification.sh
```

**预期输出**（问题已解决）:
```
✓ 文件创建成功！
✓ 文件确实存在于文件系统中
✓ 文件内容验证成功
```

#### 方法 2: 测试 Buck2 构建

```bash
# 1. 挂载 Antares overlay
cd scorpio
cargo run --bin mount_test -- --config-path scorpio.toml

# 2. 在挂载点上运行 Buck2
cd /tmp/antares_test_*/mnt/third-party/buck-hello
buck2 build //...
```

**预期结果**（问题已解决）:
```
BUILD SUCCEEDED
```

#### 方法 3: 查看调试日志

启用 debug 日志查看 `getattr_with_mapping` 的调用：

```bash
cd scorpio
RUST_LOG=debug sudo -E cargo run --bin verify_getattr_issue 2>&1 | tee /tmp/debug.log

# 查看 getattr_with_mapping 调用
grep "getattr_with_mapping" /tmp/debug.log
```

**预期输出**（问题已解决）:
```
[Dicfuse::getattr_with_mapping] inode=1, handle=None, mapping=false
[Dicfuse::getattr_with_mapping] Success: inode=1, mode=0o40755, size=0
```

## 诊断检查清单

### ✅ 检查项 1: 方法是否存在

```bash
cd scorpio
grep -c "async fn getattr_with_mapping" src/dicfuse/mod.rs
```

**预期**: 输出 `1`（方法存在）

### ✅ 检查项 2: 方法签名是否正确

```bash
cd scorpio
grep -A 3 "async fn getattr_with_mapping" src/dicfuse/mod.rs | grep "mapping: bool"
```

**预期**: 包含 `mapping: bool` 参数

### ✅ 检查项 3: 方法是否返回 ENOSYS

```bash
cd scorpio
grep -A 10 "async fn getattr_with_mapping" src/dicfuse/mod.rs | grep "ENOSYS"
```

**预期**: 不应该有 `ENOSYS`（如果有，说明方法未实现）

### ✅ 检查项 4: libfuse-fs 版本

```bash
cd scorpio
grep "libfuse-fs" Cargo.toml
```

**预期**: 应该是 `libfuse-fs = "0.1.9"` 或更高版本

### ✅ 检查项 5: 单元测试是否通过

```bash
cd scorpio
cargo test --lib dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size
```

**预期**: 测试通过

## 错误原因分析

### 根本原因

1. **OverlayFS copy-up 依赖**: OverlayFS 在执行 copy-up 操作时，必须调用 lower layer 的 `getattr_with_mapping` 来获取文件的原始属性（UID/GID/mode/size）

2. **方法缺失**: 如果 Dicfuse 没有实现 `getattr_with_mapping`，Layer trait 的默认实现会返回 `ENOSYS`

3. **Copy-up 失败**: `ENOSYS` 导致 copy-up 操作失败

4. **文件创建失败**: Copy-up 失败导致所有需要 copy-up 的文件操作失败

5. **SQLite 初始化失败**: Buck2 尝试创建 SQLite 数据库文件时，copy-up 失败，导致文件创建失败

6. **xShmMap 错误**: SQLite 收到 I/O 错误，根据上下文报告为 xShmMap 错误

### 错误传播链

```
Buck2 初始化
  └── SQLite 创建 .buck2/daemon_state.db
      └── SQLite 需要创建 .shm 文件
          └── 系统调用 creat()
              └── FUSE: OverlayFS::create
                  └── 触发 copy-up（父目录在 lower layer）
                      └── OverlayFS::copy_node_up
                          └── OverlayFS::create_upper_dir
                              └── lower_layer.getattr_with_mapping(..., false)
                                  └── 如果未实现 → 返回 ENOSYS
                                      └── copy-up 失败
                                          └── 文件创建失败
                                              └── SQLite 收到 I/O 错误
                                                  └── Buck2 报 "xShmMap I/O error"
```

## 解决方案

### 如果 getattr_with_mapping 未实现

1. **实现方法**: 在 `scorpio/src/dicfuse/mod.rs` 中实现 `getattr_with_mapping` 方法

2. **参考实现**: 查看当前实现（如果已存在）:
```bash
cd scorpio
cat src/dicfuse/mod.rs | grep -A 70 "async fn getattr_with_mapping"
```

3. **验证实现**: 运行测试确保方法正常工作

### 如果已实现但仍有问题

1. **检查方法实现**: 确保方法能够正确处理所有情况
2. **查看调试日志**: 启用 debug 日志查看方法调用情况
3. **检查 libfuse-fs 版本**: 确保使用 0.1.9 或更高版本

## 快速验证脚本

创建 `quick_check.sh`:

```bash
#!/bin/bash
cd scorpio

echo "=== 检查 getattr_with_mapping 实现 ==="
echo ""

# 检查方法是否存在
if grep -q "async fn getattr_with_mapping" src/dicfuse/mod.rs; then
    echo "✓ 方法存在"
else
    echo "✗ 方法不存在"
    exit 1
fi

# 检查方法签名
if grep -A 3 "async fn getattr_with_mapping" src/dicfuse/mod.rs | grep -q "mapping: bool"; then
    echo "✓ 方法签名正确"
else
    echo "✗ 方法签名不正确（缺少 mapping 参数）"
    exit 1
fi

# 检查是否返回 ENOSYS
if grep -A 10 "async fn getattr_with_mapping" src/dicfuse/mod.rs | grep -q "ENOSYS"; then
    echo "✗ 方法返回 ENOSYS（未实现）"
    exit 1
else
    echo "✓ 方法已实现（不返回 ENOSYS）"
fi

# 运行测试
echo ""
echo "运行单元测试..."
if cargo test --lib dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size --quiet 2>&1 | grep -q "ok"; then
    echo "✓ 单元测试通过"
else
    echo "✗ 单元测试失败"
    exit 1
fi

echo ""
echo "=== 所有检查通过 ==="
echo "getattr_with_mapping 已正确实现，Buck2 SQLite xShmMap 错误应该已解决"
```

运行:
```bash
chmod +x quick_check.sh
./quick_check.sh
```

## 总结

**验证 Buck2 SQLite xShmMap 错误的根本原因**:

1. ✅ 检查 `getattr_with_mapping` 是否已实现
2. ✅ 验证 copy-up 操作是否正常
3. ✅ 测试 Buck2 构建是否成功
4. ✅ 查看调试日志确认方法调用

**如果方法未实现**: 这就是导致错误的根本原因，需要实现该方法。

**如果方法已实现**: 问题应该已解决，可以进行实际的 Buck2 构建测试来验证。

