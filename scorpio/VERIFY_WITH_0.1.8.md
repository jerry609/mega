# 使用 0.1.8 版本进行实际验证

## 🎯 目标

通过切换到 libfuse-fs 0.1.8 版本并添加详细日志，直接验证我们的假设。

## 📋 验证计划

### 步骤 1: 切换到 0.1.8 并测试

```bash
./scripts/test_with_0.1.8.sh
```

**这个脚本会**:
1. 备份当前的 Cargo.toml
2. 修改 libfuse-fs 版本为 0.1.8
3. 检查当前是否有 do_getattr_helper 实现
4. 尝试构建（预期会失败或使用默认实现）
5. 运行测试观察行为
6. 自动恢复环境

**预期结果**:
- ✗ 如果构建失败：说明 API 不兼容（当前用了 getattr_with_mapping）
- ✓ 如果构建成功但测试失败：说明使用了默认实现（返回 ENOSYS）

### 步骤 2: 添加详细调试日志

```bash
./scripts/add_debug_logs.sh
```

**这个脚本会**:
1. 备份 src/dicfuse/mod.rs
2. 检查当前的日志实现
3. 提供添加日志的建议
4. 显示建议的日志策略

**建议的日志点**:
```rust
// 入口
tracing::info!("🔵 [ENTER] Dicfuse::getattr_with_mapping");
tracing::debug!("   inode={}, mapping={}", inode, mapping);

// 关键步骤
tracing::debug!("🔵 [STEP 1] Calling store.get_inode({})", inode);
tracing::debug!("🔵 [STEP 2] Got item, constructing stat64...");

// 成功
tracing::info!("🟢 [EXIT] SUCCESS: mode={:#o}, size={}", mode, size);

// 失败
tracing::error!("🔴 [ERROR] Failed: {:?}", e);
```

### 步骤 3: 运行测试观察日志

```bash
# 启用详细日志
export RUST_LOG="scorpio=trace,libfuse_fs=debug"

# 运行测试
cargo test --test test_copy_up_chain -- --nocapture
```

**观察要点**:
1. 是否看到 `[ENTER] Dicfuse::getattr_with_mapping`？
   - ✓ 看到 → 方法被调用
   - ✗ 没看到 → 方法未被调用或使用了默认实现

2. 是否看到 `[EXIT] SUCCESS`？
   - ✓ 看到 → 方法成功执行
   - ✗ 没看到 → 方法执行失败

3. 是否看到错误日志？
   - 如果看到 "DEFAULT IMPL CALLED" → 使用了默认实现
   - 如果看到 "Failed to get inode" → inode 不存在
   - 如果看到 "ENOSYS" → 功能未实现

## 🔍 验证场景

### 场景 A: 0.1.8 + 当前代码（有 getattr_with_mapping）

```bash
./scripts/test_with_0.1.8.sh
```

**预期**: 构建失败

**原因**: 
- 当前代码实现了 `getattr_with_mapping` (0.1.9 的方法)
- 0.1.8 的 Layer trait 没有这个方法
- 编译器报错：method not found in trait

**结论**: 这证明了 API 不兼容

### 场景 B: 0.1.8 + 没有任何实现

如果我们临时移除 `getattr_with_mapping`：

```bash
# 备份
cp src/dicfuse/mod.rs src/dicfuse/mod.rs.backup

# 注释掉 getattr_with_mapping 实现
# 然后构建
cargo build
```

**预期**: 构建成功，但运行时失败

**原因**:
- 编译通过（使用 Layer trait 默认实现）
- 运行时默认实现返回 ENOSYS
- Copy-up 失败

**验证方法**:
```bash
RUST_LOG=debug cargo test --test test_copy_up_chain -- --nocapture 2>&1 | grep -E "ENOSYS|getattr"
```

**预期输出**:
```
code: 38, kind: Unsupported, message: "Function not implemented"
```

### 场景 C: 0.1.9 + 当前代码（有 getattr_with_mapping）

这是当前的状态：

**结果**: ✓ 一切正常

**原因**:
- API 匹配
- 方法被正确调用
- Copy-up 成功

## 📊 验证结果对照表

| 场景 | libfuse-fs 版本 | Dicfuse 实现 | 构建结果 | 运行结果 | 说明 |
|------|----------------|-------------|---------|---------|------|
| A | 0.1.8 | getattr_with_mapping | ✗ 失败 | N/A | API 不匹配 |
| B | 0.1.8 | 无实现 | ✓ 成功 | ✗ 失败 (ENOSYS) | 使用默认实现 |
| C | 0.1.8 | do_getattr_helper | ✓ 成功 | ✓ 成功 | 正确实现 |
| D | 0.1.9 | getattr_with_mapping | ✓ 成功 | ✓ 成功 | 正确实现（当前状态）|
| E | 0.1.9 | 无实现 | ✓ 成功 | ✗ 失败 (ENOSYS) | 使用默认实现 |

## 🎯 最关键的验证

要验证我们的假设（0.1.8 时代 Dicfuse 没有实现 do_getattr_helper），最直接的方法是：

### 方法 1: 检查 git 历史

```bash
# 查看 0.1.8 时代的代码
git log --all --oneline --follow -- scorpio/Cargo.toml | grep -B 5 -A 5 "0.1.8"

# 找到使用 0.1.8 的提交，查看当时的 dicfuse/mod.rs
git show <commit>:scorpio/src/dicfuse/mod.rs | grep -A 20 "do_getattr_helper"
```

### 方法 2: 实际回退测试

```bash
# 1. 找到使用 0.1.8 的提交
COMMIT_0_1_8=$(git log --all --oneline -- scorpio/Cargo.toml | grep "0.1.8" | head -1 | cut -d' ' -f1)

# 2. 检出到那个提交
git checkout $COMMIT_0_1_8

# 3. 查看是否有实现
grep -n "do_getattr_helper" scorpio/src/dicfuse/mod.rs

# 4. 尝试构建和测试
cargo build
cargo test

# 5. 回到当前分支
git checkout -
```

### 方法 3: 使用日志追踪（推荐）

这是最安全的方法，不需要修改版本：

1. **在当前版本添加详细日志**
2. **运行测试观察调用链路**
3. **确认方法是否被正确调用**

```bash
# 添加日志
./scripts/add_debug_logs.sh

# 手动在 src/dicfuse/mod.rs 中添加日志（按脚本建议）

# 运行测试
RUST_LOG=scorpio=trace cargo test --test test_copy_up_chain -- --nocapture
```

## ✅ 预期发现

如果我们的假设正确，应该看到：

### 在 0.1.8 时代（如果能回退）:
```
[ERROR] Layer trait default implementation called
[ERROR] Returning ENOSYS (Function not implemented)
[ERROR] Copy-up failed: Os { code: 38, ... }
```

### 在当前版本（0.1.9）:
```
[INFO] 🔵 [ENTER] Dicfuse::getattr_with_mapping
[DEBUG]    inode=123, mapping=false
[DEBUG] 🔵 [STEP 1] Calling store.get_inode(123)
[DEBUG] 🔵 [STEP 2] Got item, constructing stat64...
[INFO] 🟢 [EXIT] SUCCESS: mode=0o100644, size=1024
```

## 🚀 快速开始

```bash
# 1. 测试当前版本的日志
RUST_LOG=scorpio=debug cargo test --test test_copy_up_chain test_error_propagation_chain -- --nocapture

# 2. 尝试切换到 0.1.8（会自动恢复）
./scripts/test_with_0.1.8.sh

# 3. 查看日志策略建议
./scripts/add_debug_logs.sh

# 4. 如果需要，检查 git 历史
git log --all --oneline --follow -- scorpio/Cargo.toml | head -20
```

## 📚 相关文档

- `scripts/test_with_0.1.8.sh` - 自动测试 0.1.8 版本
- `scripts/add_debug_logs.sh` - 日志添加指南
- `DEBUG_GUIDE.md` - 完整调试指南
- `doc/FINAL_ROOT_CAUSE.md` - 根本原因分析

