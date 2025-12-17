# 调试指南：验证 Copy-up 调用链路

## 🎯 目标

验证完整的 OverlayFS copy-up 调用链路，确认：
1. `getattr_with_mapping` 是否被正确调用
2. 错误如何传播
3. 为什么 0.1.8 版本会失败

## 🔧 调试工具

### 1. 自动化调试脚本

```bash
# 使用 overlayfs 演示完整的调用链路
sudo ./scripts/debug_call_chain.sh
```

**脚本功能**:
- 创建测试环境（upper/lower/work/mnt）
- 演示 copy-up 触发条件
- 使用 strace 追踪系统调用
- 模拟 Buck2 SQLite 场景
- 显示完整的调用链路图

### 2. 单元测试

```bash
# 运行调用链路测试
cargo test --test test_copy_up_chain -- --nocapture

# 运行特定测试（需要实际的 store）
cargo test --test test_copy_up_chain test_getattr_with_mapping_call_chain --ignored -- --nocapture
```

**测试内容**:
- 验证 `getattr_with_mapping` 是否正确实现
- 模拟 copy-up 场景
- 测试错误传播链
- 验证不同 mapping 参数的行为

### 3. 手动调试

#### 步骤 1: 启用详细日志

```bash
export RUST_LOG="scorpio=debug,libfuse_fs=debug"
export RUST_BACKTRACE=1
```

#### 步骤 2: 运行 Antares

```bash
cargo run --bin scorpio -- mount /path/to/mountpoint
```

#### 步骤 3: 触发 copy-up

```bash
# 在另一个终端
cd /path/to/mountpoint

# 读取文件（不触发 copy-up）
cat some_file.txt

# 写入文件（触发 copy-up）
echo "modified" >> some_file.txt
```

#### 步骤 4: 查看日志

查找关键日志：
- `[Dicfuse::getattr_with_mapping]` - 方法被调用
- `Success: inode=...` - 成功返回
- `Failed to get inode` - 失败

### 4. 使用 strace 追踪

```bash
# 追踪 FUSE 操作
strace -f -e trace=getxattr,stat,lstat,fstat,open,openat,write \
    -o /tmp/fuse_trace.log \
    cargo run --bin scorpio -- mount /path/to/mountpoint
```

在另一个终端触发操作，然后查看 `/tmp/fuse_trace.log`。

## 📋 完整的调用链路

### 在 Antares/Dicfuse 场景中

```
用户操作: echo "text" >> /mnt/file.txt
  │
  ▼
FUSE 内核: FUSE_WRITE 请求
  │
  ▼
OverlayFS (libfuse-fs)::write()
  │
  ├─ 检查文件是否在 upper layer
  │  └─ 不在 → 需要 copy-up
  │
  ▼
OverlayFS::copy_node_up()
  │
  └─ 对于文件: copy_regfile_up()
     │
     ├─ 📍 关键调用点 1:
     │  lower_layer.getattr_with_mapping(inode, None, false)
     │  │
     │  └─ Dicfuse::getattr_with_mapping()
     │     │
     │     ├─ store.get_inode(inode)
     │     │  └─ 获取 StorageItem
     │     │
     │     ├─ item.get_stat()
     │     │  └─ 获取 FileAttr
     │     │
     │     ├─ 构造 libc::stat64
     │     │  ├─ st_ino = inode
     │     │  ├─ st_mode = type_bits | perm
     │     │  ├─ st_uid = attr.uid
     │     │  ├─ st_gid = attr.gid
     │     │  ├─ st_size = file_len
     │     │  └─ ...
     │     │
     │     └─ Ok((stat, Duration::from_secs(2)))
     │
     ├─ 在 upper layer 创建文件
     │  └─ upper_layer.create_with_context(...)
     │     └─ PassthroughFS 创建实际文件
     │
     └─ 复制文件内容
        ├─ lower_layer.read(...)
        │  └─ Dicfuse 读取数据
        │
        └─ upper_layer.write(...)
           └─ PassthroughFS 写入数据
```

### 错误传播链（如果未实现）

```
Dicfuse::getattr_with_mapping 未实现
  │
  └─ Layer trait 默认实现被调用
     │
     └─ 返回 Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
        │
        ▼
OverlayFS::copy_regfile_up 收到错误
  │
  └─ .await? 传播错误
     │
     ▼
OverlayFS::copy_node_up 收到错误
  │
  └─ .await? 传播错误
     │
     ▼
OverlayFS::write 失败
  │
  └─ 返回错误给 FUSE 内核
     │
     ▼
FUSE 内核返回错误给应用
  │
  └─ write() 系统调用失败
     │
     ▼
应用（如 SQLite）收到 I/O 错误
  │
  └─ SQLite: "I/O error within the xShmMap method"
     │
     ▼
Buck2 报告错误并退出
```

## 🔍 关键调试点

### 1. 检查方法是否被调用

在日志中查找：
```
[Dicfuse::getattr_with_mapping] inode=..., handle=..., mapping=...
```

如果没有看到这行日志，说明：
- 方法未被调用（OverlayFS 路径问题）
- 或者日志级别不够

### 2. 检查返回值

在日志中查找：
```
[Dicfuse::getattr_with_mapping] Success: inode=..., mode=..., size=...
```

如果看到 `Failed to get inode`，说明：
- inode 不存在
- store 有问题

### 3. 检查 copy-up 是否触发

```bash
# 检查 upper layer 是否有文件
ls -la /path/to/upper/

# 如果文件在 upper layer，说明 copy-up 成功
# 如果没有，说明 copy-up 失败或未触发
```

### 4. 检查错误码

如果看到错误，检查错误码：
- `ENOSYS` (38): Function not implemented - 方法未实现
- `ENOENT` (2): No such file or directory - 文件不存在
- `EPERM` (1): Operation not permitted - 权限问题
- `EIO` (5): Input/output error - I/O 错误

## 📊 验证清单

- [ ] `getattr_with_mapping` 方法已实现
- [ ] 方法签名正确（包括 `mapping: bool` 参数）
- [ ] 方法被正确调用（查看日志）
- [ ] 方法返回正确的 stat 信息
- [ ] Copy-up 操作成功
- [ ] 文件可以正常写入
- [ ] SQLite 数据库可以创建
- [ ] Buck2 构建成功

## 🎯 预期结果

### 正确实现时

```
[Dicfuse::getattr_with_mapping] inode=123, handle=None, mapping=false
[Dicfuse::getattr_with_mapping] Success: inode=123, mode=0o100644, size=1024, uid=1000, gid=1000
```

copy-up 成功，文件可以写入。

### 未实现或实现错误时

```
Error: Os { code: 38, kind: Uncategorized, message: "Function not implemented" }
```

或者：

```
[Dicfuse::getattr_with_mapping] Failed to get inode 123: ...
```

copy-up 失败，文件无法写入，Buck2 报错。

## 📚 相关文档

- `doc/FINAL_ROOT_CAUSE.md` - 根本原因分析
- `doc/IMPLEMENTATION_COMPARISON.md` - 实现对比
- `doc/libfuse-fs-version-deep-dive.md` - 源码深度分析
- `VALIDATION_SUMMARY.md` - 验证总结

## 🚀 快速开始

```bash
# 1. 运行验证脚本
cd scorpio
./scripts/verify_root_cause_hypothesis.sh

# 2. 运行调试脚本（需要 root）
sudo ./scripts/debug_call_chain.sh

# 3. 运行单元测试
cargo test --test test_copy_up_chain -- --nocapture

# 4. 启用详细日志运行 Antares
RUST_LOG=debug cargo run --bin scorpio -- mount /mnt/antares
```

