## Buck2 在 Antares/Dicfuse 挂载上的构建问题复盘

### 背景
- 目标：在 Antares Overlay 挂载点（下层 Dicfuse 只读，上层 Passthrough 可写）执行 `buck2 build`，验证 Buck2 能否在挂载后正常编译 `third-party/buck-hello`。
- 运行环境：Linux，root 权限；分支 `feature/dicfuse-global-singleton`。
- 挂载流程：测试用例 `antares::fuse::tests::test_run_mount` 或工具 `bin/mount_and_build.rs` 自动创建 `/tmp/antares_build_*` 目录，装配 overlay 并挂载到 `/tmp/antares_build_*/mnt`。

### 测试过程（时间序列）
1. **初始尝试（mount_and_build）**
   - 入口：`cargo run -p scorpio --bin mount_and_build -- --config-path scorpio/scorpio.toml --build-rel third-party/buck-hello --target //...`
   - 现象：Buck2 报错 “buck2 is not allowed to run as root” → 通过 `HOME=/root` + `BUCK2_ALLOW_ROOT=1` 解决。

2. **Dicfuse 载入与挂载**
   - Dicfuse `import_arc` 完成，目录树加载成功；挂载成功但 `readdir` 偶有 200ms 超时告警（仍可继续）。

3. **Buck2 运行阶段反复失败**
   - 错误核心：`Error initializing DaemonStateData` → `disk I/O error` → `Error code 5386: I/O error within the xShmMap method (trying to map a shared-memory segment into process address space)`。
   - 尝试的缓解措施：
     - 将 Buck2 daemon / isolation / tmp / buck-out 迁移至非 FUSE 路径 `/tmp/buck2_daemon{/,/isolation,/tmp,/buck-out}`。
     - 设置环境：`BUCK2_DAEMON_DIR`、`BUCK2_ISOLATION_DIR`、`TMPDIR`、`BUCK_OUT`。
     - 去掉不被支持的 CLI 参数（`--isolation-dir`、`--buck-out`）。
   - 结果：仍然在挂载工作区内创建/使用 SQLite 状态文件，xShmMap 在 FUSE 上失败，Buck2 退出码 11。

4. **手工进入挂载验证（预期）**
   - 即便进入 `/tmp/antares_test_mount_*/mnt/third-party/buck-hello` 手工执行同样的 Buck2 命令，也会复现相同的 SQLite shm I/O error，因为状态文件依然落在挂载目录下。

### 关键日志摘录
```
Command failed: Error initializing DaemonStateData
Caused by:
  0: creating sqlite table materializer_state
  1: disk I/O error
  2: Error code 5386: I/O error within the xShmMap method (trying to map a shared-memory segment into process address space)
```

### 问题分析
- Buck2 在初始化 DaemonStateData 时会在仓库根生成 SQLite 文件并使用共享内存（WAL/SHM）。
- FUSE（Antares Overlay）对 SQLite 的 shm/mmap 访问存在兼容性问题，导致 xShmMap 返回 I/O error。
- 即便将 daemon/isolation/tmp/buck-out 重定向到 `/tmp`，Buck2 仍在工作区（挂载内）生成某些状态文件，无法完全避免。
- Buck2 官方文档未公开关闭 shm 或完全迁移状态目录的参数；未找到禁用 WAL/SHM 的 Buck2 选项。

### 结论
- **在当前 Overlay 挂载上直接运行 Buck2 构建不可行**：会稳定触发 SQLite xShmMap I/O error。
- 问题不在 Dicfuse 挂载流程（读写/拷贝均正常），而在 Buck2 对 FUSE 的 SQLite 共享内存依赖。

### 可行绕过方案
1. **在本地磁盘构建**（推荐）：将 `third-party/buck-hello` 从挂载点 `rsync/复制` 到非 FUSE 路径（如 `/tmp/buck_hello_work`），在本地目录执行 `buck2 build //...`。
2. **仅在挂载内做只读浏览**：构建、测试在本地磁盘完成，避免 FUSE 上的 SQLite shm。
3. **若有 Buck2 内部参数或补丁**：
   - 若能强制 Buck2 将所有状态（含 materializer_state）放到本地目录，或禁用 shm/WAL，可能解决；但未在公开文档找到相关参数，需要 Buck2 侧支持。

### 复现指令（供交接）
```bash
# 启动挂载并阻塞等待（root）
sudo -E cargo test -p scorpio --lib antares::fuse::tests::test_run_mount -- --exact --ignored --nocapture

# 另开终端进入挂载点运行 Buck2（预期失败，xShmMap I/O error）
cd /tmp/antares_test_mount_*/mnt/third-party/buck-hello
BUCK2_ALLOW_ROOT=1 \
BUCK2_DAEMON_DIR=/tmp/buck2_daemon \
BUCK2_ISOLATION_DIR=/tmp/buck2_daemon/isolation \
TMPDIR=/tmp/buck2_daemon/tmp \
BUCK_OUT=/tmp/buck2_daemon/buck-out \
buck2 build //...
```

### 状态
- 挂载/读写/Copy-Up：通过。
- Buck2 在挂载内构建：失败，阻塞于 SQLite xShmMap I/O error。
