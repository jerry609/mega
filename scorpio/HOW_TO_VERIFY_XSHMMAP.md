# 如何验证 Buck2 SQLite xShmMap 错误的根本原因

## 🎯 快速验证（推荐）

运行快速检查脚本：

```bash
cd scorpio
./scripts/quick_check_getattr.sh
```

**如果所有检查通过** ✅:
- `getattr_with_mapping` 已正确实现
- Buck2 SQLite xShmMap 错误应该已解决
- 可以进行实际的 Buck2 构建测试

**如果检查失败** ❌:
- 问题确实是由 `getattr_with_mapping` 缺失导致的
- 需要实现该方法（参考 `VERIFY_XSHMMAP_ERROR.md`）

## 📋 详细验证步骤

### 步骤 1: 检查方法是否实现

```bash
cd scorpio

# 方法 1: 查看源码
grep -A 10 "async fn getattr_with_mapping" src/dicfuse/mod.rs

# 方法 2: 运行单元测试
cargo test --lib dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size
```

**预期结果**（已实现）:
- 看到完整的方法实现
- 单元测试通过

**如果未实现**:
- 只看到 Layer trait 的默认实现
- 或者方法不存在
- 单元测试失败

### 步骤 2: 验证 copy-up 操作

```bash
# 挂载 Antares overlay（需要 root）
cd scorpio
sudo cargo run --bin mount_test -- --config-path scorpio.toml

# 在另一个终端，尝试创建文件
cd /tmp/antares_test_*/mnt/third-party/buck-hello
touch test_file.txt
```

**如果 getattr_with_mapping 未实现** ❌:
```
touch: cannot touch 'test_file.txt': Function not implemented
```

**如果已实现** ✅:
```
# 文件创建成功，无错误
```

### 步骤 3: 测试 Buck2 构建

```bash
# 挂载 Antares overlay
cd scorpio
cargo run --bin mount_test -- --config-path scorpio.toml

# 在挂载点上运行 Buck2
cd /tmp/antares_test_*/mnt/third-party/buck-hello
buck2 build //...
```

**如果 getattr_with_mapping 未实现** ❌:
```
Error code 5386: I/O error within the xShmMap method
```

**如果已实现** ✅:
```
BUILD SUCCEEDED
```

## 🔍 复现问题（验证根本原因）

如果需要验证问题确实是由 `getattr_with_mapping` 缺失导致的：

```bash
cd scorpio
./scripts/reproduce_xshmmap_error.sh
```

这个脚本会：
1. 临时禁用 `getattr_with_mapping` 方法
2. 重新编译
3. 指导你测试 Buck2 构建
4. 应该会看到 SQLite xShmMap 错误
5. 自动恢复原始实现

## 📊 诊断检查清单

### ✅ 检查项 1: 方法是否存在

```bash
cd scorpio
grep -c "async fn getattr_with_mapping" src/dicfuse/mod.rs
# 预期: 输出 1
```

### ✅ 检查项 2: 方法签名是否正确

```bash
cd scorpio
grep -A 5 "async fn getattr_with_mapping" src/dicfuse/mod.rs | grep "mapping"
# 预期: 看到 mapping: bool 或 _mapping: bool
```

### ✅ 检查项 3: 方法是否返回 ENOSYS

```bash
cd scorpio
grep -A 15 "async fn getattr_with_mapping" src/dicfuse/mod.rs | grep "ENOSYS"
# 预期: 不应该有输出（如果有，说明方法未实现）
```

### ✅ 检查项 4: libfuse-fs 版本

```bash
cd scorpio
grep "libfuse-fs" Cargo.toml
# 预期: libfuse-fs = "0.1.9" 或更高
```

### ✅ 检查项 5: 单元测试是否通过

```bash
cd scorpio
cargo test --lib dicfuse::tests::test_getattr_with_mapping_preserves_mode_and_size
# 预期: test ... ok
```

## 🐛 错误原因分析

### 根本原因

1. **OverlayFS copy-up 依赖**: OverlayFS 在执行 copy-up 时必须调用 `getattr_with_mapping`
2. **方法缺失**: 如果未实现，默认返回 `ENOSYS`
3. **Copy-up 失败**: `ENOSYS` 导致 copy-up 失败
4. **文件创建失败**: Copy-up 失败导致文件操作失败
5. **SQLite 错误**: Buck2 创建 SQLite 文件时失败，报告为 xShmMap 错误

### 错误传播链

```
Buck2 → SQLite → 创建文件 → FUSE → OverlayFS → copy-up 
→ getattr_with_mapping (未实现) → ENOSYS 
→ copy-up 失败 → 文件创建失败 → SQLite I/O 错误 
→ Buck2 报 "xShmMap I/O error"
```

## ✅ 解决方案

### 如果方法未实现

1. **实现方法**: 参考 `src/dicfuse/mod.rs:101-166` 的实现
2. **验证实现**: 运行单元测试
3. **测试 Buck2**: 进行实际的构建测试

### 如果方法已实现

1. **验证功能**: 运行快速检查脚本
2. **测试构建**: 进行实际的 Buck2 构建测试
3. **查看日志**: 如果仍有问题，启用 debug 日志查看详情

## 📝 相关文档

- `VERIFY_XSHMMAP_ERROR.md` - 详细的验证指南
- `DEBUG_STATUS.md` - 调试状态总结
- `README_VERIFICATION.md` - 验证工具说明

## 🚀 快速开始

**最快的方式**:

```bash
cd scorpio

# 1. 快速检查
./scripts/quick_check_getattr.sh

# 2. 如果检查通过，测试 Buck2
cargo run --bin mount_test -- --config-path scorpio.toml
# 在另一个终端
cd /tmp/antares_test_*/mnt/third-party/buck-hello
buck2 build //...
```

**如果检查失败，查看详细指南**:

```bash
cat VERIFY_XSHMMAP_ERROR.md
```

