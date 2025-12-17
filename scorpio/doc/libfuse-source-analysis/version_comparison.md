# libfuse-fs 0.1.8 vs 0.1.9 版本对比

## API 变更

### Layer Trait

| 版本 | 方法名 | 签名 |
|------|--------|------|
| 0.1.8 | `do_getattr_helper` | `async fn do_getattr_helper(&self, inode: Inode, handle: Option<u64>) -> Result<(stat64, Duration)>` |
| 0.1.9 | `getattr_with_mapping` | `async fn getattr_with_mapping(&self, inode: Inode, handle: Option<u64>, mapping: bool) -> Result<(stat64, Duration)>` |

### OverlayFS Copy-up 调用

| 操作 | 0.1.8 版本 | 0.1.9 版本 |
|------|-----------|-----------|
| create_upper_dir | `do_getattr_helper(inode, None)` | `getattr_with_mapping(inode, None, false)` |
| copy_regfile_up | `do_getattr_helper(inode, None)` | `getattr_with_mapping(inode, None, false)` |
| copy_symlink_up | `do_getattr_helper(inode, None)` | `getattr_with_mapping(inode, None, false)` |

## 语义变更

- **0.1.8**: `do_getattr_helper` = "绕过 ID 映射，获取原始属性"
- **0.1.9**: `getattr_with_mapping(..., false)` = "mapping=false，获取未映射的原始属性"

两者功能相同，但 0.1.9 的 API 更清晰，支持可选的 ID 映射控制。

## 迁移指南

如果从 0.1.8 升级到 0.1.9：

1. 将 `do_getattr_helper` 重命名为 `getattr_with_mapping`
2. 添加 `mapping: bool` 参数
3. 对于只读层（如 Dicfuse），可以忽略 `mapping` 参数
4. 对于需要 ID 映射的层，根据 `mapping` 参数决定是否应用映射
