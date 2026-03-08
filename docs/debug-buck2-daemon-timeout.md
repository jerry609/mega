# Buck2 Daemon Timeout on Antares FUSE Mount — 调试与修复文档

## 问题描述

在 Antares (scorpio) FUSE 挂载的仓库上执行 buck2 构建时，buck2 daemon 启动过程中对整个目录树执行 `statx` 系统调用扫描，由于 Antares 需要逐个目录向远程服务器请求元数据，导致响应极慢，最终 buck2 客户端显示超时并杀死 daemon。

### 错误日志

```
[2026-02-24T03:22:48Z ERROR libfuse_fs::passthrough::file_handle] open_by_handle_at failed error Os { code: 116, kind: StaleNetworkFileHandle, message: "Stale file handle" }
...
[2026-02-24T03:24:46Z] Buck2 daemon pid 1232056 has exited
[2026-02-24T03:24:46.239+00:00] Starting new buck2 daemon...
[2026-02-24T03:24:46Z ERROR libfuse_fs::passthrough::file_handle] open_by_handle_at failed error Os { code: 116, kind: StaleNetworkFileHandle, message: "Stale file handle" }
...
command failed: Failed to connect to buck daemon
Try running `buck2 kill` and your command afterwards.
```

### 复现步骤

1. 通过 Antares API 挂载一个 monorepo 目录（如 `/`）
2. 在挂载点对应的 buck2 项目目录执行 `buck2 build`
3. 观察 strace 可以看到 daemon 不断发出 `statx` 调用，直到客户端超时

---

## 根因分析

### 问题链路（5 层）

```
buck2 daemon startup
  → statx() 扫描整个目录树（数千个文件/目录）
    → 每个 statx → Linux 内核 FUSE 模块
      → rfuse3 dispatch → OverlayFs → Dicfuse lookup/getattr
        → 若目录未加载 → ensure_dir_loaded() → HTTP GET /api/v1/tree/content-hash
          → 每个目录一次网络请求（RTT 10-100ms）
            → 数百个目录 × 100ms = 数十秒到数分钟
```

### 1. Buck2 的 statx 风暴

Buck2 启动时会通过 `statx` 系统调用遍历整个项目目录树，目的是建立文件系统状态快照。这是 Buck2 的正常行为——在普通文件系统上几乎无感，但在 FUSE 上每次 statx 都要回到用户态 daemon。

用 strace 观察：
```bash
strace -e statx -p <buck2_daemon_pid>
# 可以看到成千上万的 statx 调用
```

### 2. Antares/Dicfuse 的逐目录网络请求

当 buck2 的 statx 到达 FUSE daemon 时，调用路径如下：

```
kernel → rfuse3 → OverlayFs::lookup/getattr
  → Dicfuse::lookup() (scorpio/src/dicfuse/async_io.rs:68)
    → store.dir_refresh_needed(parent) → 检查目录是否已加载
    → store.ensure_dir_loaded(parent) → 未加载的目录需要网络请求
      → fetch_dir() (scorpio/src/dicfuse/store.rs:463) → HTTP 请求
```

**关键代码位置：`scorpio/src/dicfuse/async_io.rs` 第 68-184 行 — lookup 实现**

```rust
// scorpio/src/dicfuse/async_io.rs:68-125
async fn lookup(&self, _req: Request, parent: Inode, name: &OsStr) -> Result<ReplyEntry> {
    const LOOKUP_REFRESH_WAIT_BUDGET_MS: u64 = 20;  // 只等 20ms
    const LOOKUP_MISS_RETRY_WAIT_BUDGET_MS: u64 = 200;  // miss 后再等 200ms

    let refresh_needed = store.dir_refresh_needed(parent);  // 检查 TTL
    if refresh_needed {
        // 后台 spawn 网络请求
        let handle = tokio::spawn(async move { store.ensure_dir_loaded(parent).await });
        // 最多等 20ms
        match tokio::time::timeout(Duration::from_millis(20), &mut handle).await {
            Err(_) => {
                // 超时了，继续走缓存
                refresh_timed_out = true;
                refresh_handle = Some(handle);
            }
            ...
        }
    }
    // 如果缓存中没有，且 refresh 超时了，再等 200ms
    if child.is_none() && refresh_timed_out { ... }
}
```

**问题**：每个未加载的目录都触发一次 `ensure_dir_loaded`，每次都是一个 HTTP 请求。虽然 lookup 有 20ms + 200ms 的超时限制不会永久阻塞，但在冷启动时数百个目录的请求加起来仍然很慢。

### 3. Stale File Handle 错误

`open_by_handle_at failed error Os { code: 116, kind: StaleNetworkFileHandle }` 来自 `libfuse-fs` 的 passthrough 层：

**代码位置：`libfuse-fs` crate，`passthrough/file_handle.rs` 第 296-315 行**

```rust
// libfuse-fs (外部 crate) passthrough/file_handle.rs
impl OpenableFileHandle {
    pub fn open(&self, flags: libc::c_int) -> io::Result<File> {
        let ret = unsafe {
            open_by_handle_at(
                self.mount_fd.as_fd().as_raw_fd(),
                self.handle.handle.wrapper.as_fam_struct_ptr(),
                flags,
            )
        };
        if ret >= 0 {
            let file = unsafe { File::from_raw_fd(ret) };
            Ok(file)
        } else {
            let e = io::Error::last_os_error();
            error!("open_by_handle_at failed error {e:?}");  // ← 就是这行日志
            Err(e)
        }
    }
}
```

**原因**：OverlayFs 的 upper layer 使用 `passthrough` 文件系统，通过 `name_to_handle_at` 获取文件句柄并缓存。当 buck2 daemon 被超时杀死后重启，之前的 file handle 已经失效（底层 FUSE mount 可能已经 unmount/remount），导致 `open_by_handle_at` 返回 `ESTALE`（errno 116）。

### 4. 恶性循环

```
buck2 daemon 启动 → statx 风暴 → FUSE 逐目录请求 → 超时
→ daemon 被杀 → 新 daemon 启动 → passthrough file handle 已失效 → ESTALE
→ 新 daemon 也超时 → 重复...
```

---

## 已有优化措施分析

### 措施 1: Lookup 非阻塞设计（当前代码）

**位置：`scorpio/src/dicfuse/async_io.rs:68-184`**

lookup 实现了"非阻塞"模式：先给 20ms 预算等网络请求，超时后走缓存，miss 后再给 200ms。这避免了单个 lookup 阻塞太久，但冷启动时大量 miss 仍然累积大量网络请求。

### 措施 2: Reply TTL 内核缓存

**位置：`scorpio/src/dicfuse/mod.rs:262-270`**

```rust
pub(crate) fn reply_ttl(&self) -> Duration {
    let is_subdir_mount = !(self.base_path().is_empty() || self.base_path() == "/");
    let ttl_secs = if is_subdir_mount {
        config::antares_dicfuse_reply_ttl_secs()  // Antares → 60秒
    } else {
        config::dicfuse_reply_ttl_secs()  // 普通 → 2秒
    };
    Duration::from_secs(ttl_secs)
}
```

**配置：`scorpio/scorpio.toml`**

| 配置项 | 普通挂载 | Antares 挂载 | 含义 |
|--------|---------|-------------|------|
| `dicfuse_reply_ttl_secs` | 2s | — | 内核 FUSE entry/attr 缓存 TTL |
| `antares_dicfuse_reply_ttl_secs` | — | 60s | Antares 挂载的内核缓存 TTL |
| `dicfuse_dir_sync_ttl_secs` | 5s | — | 应用层目录刷新间隔 |
| `antares_dicfuse_dir_sync_ttl_secs` | — | 120s | Antares 目录刷新间隔 |

TTL 从 `ReplyEntry.ttl` 字段传给内核，内核在 TTL 内对相同 inode 的 lookup/getattr 直接返回缓存值，不回到 FUSE daemon。

**但问题是**：第一次访问（冷启动）没有缓存，所有 statx 都会穿透到 daemon。

### 措施 3: Preheat（预热）

**位置：`orion/src/buck_controller.rs:421-481`**

```rust
// 全量预热：ls -lR（遍历整个目录树）
fn preheat(repo_path: &Path) -> anyhow::Result<()> {
    std::process::Command::new("ls")
        .arg("-lR")
        .current_dir(repo_path)
        .status()?;
    Ok(())
}

// 浅层预热：只预热 N 层深度
fn preheat_shallow(repo_path: &Path, max_depth: usize) -> anyhow::Result<()> {
    let mut stack = vec![(repo_path.to_path_buf(), 0usize)];
    while let Some((path, depth)) = stack.pop() {
        for entry in std::fs::read_dir(&path)? {
            let _ = entry.metadata(); // 触发 FUSE getattr 缓存
            if file_type.is_dir() && depth < max_depth {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
}
```

`preheat` 通过 `ls -lR` 预热 FUSE 缓存，但本身也需要遍历整个目录树，同样会触发大量网络请求。如果 Dicfuse 的 `import_arc` 还未完成，preheat 就是在冷缓存上遍历，不如等 import 完成。

### 措施 4: import_arc 后台预加载

**位置：`scorpio/src/dicfuse/store.rs:2300-2390`**

`import_arc` 在 FUSE 挂载时后台异步加载目录树到指定深度（`load_dir_depth`）。对于 Antares 挂载，默认 `antares_load_dir_depth = 3`，会预加载前 3 层目录。

**问题**：如果 buck2 build 在 `import_arc` 完成之前就开始，则 preheat 和 buck2 都面临冷缓存。

### 措施 5: write_back cache mount option

**位置：`scorpio/src/server/mod.rs:62-70`**

```rust
fn apply_antares_cache_mount_options(options: &mut MountOptions) {
    options.write_back(true);  // 协商 FUSE_WRITEBACK_CACHE
}
```

`write_back` 只影响写入缓存策略（合并多次 write 为一次），对 statx/lookup 的读缓存没有帮助。

### 措施 6: 曾尝试的 ANTARES_FUSE_CACHE_MOUNT_OPTIONS（已删除）

**PR #1913 (`8cef669c`) 曾添加：**

```rust
const ANTARES_FUSE_CACHE_MOUNT_OPTIONS: &str =
    "kernel_cache,auto_cache,entry_timeout=60,attr_timeout=60,negative_timeout=10";
```

这些是 libfuse2 风格的挂载选项，在 rfuse3 (FUSE3) 中不被识别，导致挂载失败。已在后续提交中删除。

**正确做法**：在 FUSE3 中，entry/attr 超时通过每次 reply 的 TTL 字段控制（即措施 2 的 `reply_ttl()`），不通过 mount options。

---

## 问题定位总结

| # | 问题 | 严重度 | 代码位置 |
|---|------|--------|---------|
| 1 | 冷启动时每个目录独立发起 HTTP 请求 | **高** | `store.rs:1131 ensure_dir_loaded()` |
| 2 | buck2 build 不等 import_arc 完成就开始 preheat | **高** | `buck_controller.rs:573 preheat()` |
| 3 | preheat 本身也是 `ls -lR` 全量遍历 | **中** | `buck_controller.rs:421-433` |
| 4 | daemon 超时被杀后 passthrough file handle 失效 | **中** | `libfuse-fs/passthrough/file_handle.rs:298` |
| 5 | readdirplus 中 get_stat 可能触发网络 IO | **低** | `async_io.rs:679 get_stat_fast()` |

---

## 修复方案（已实施）

本次修复采用 **方案 1+3** 组合：在 Scorpio 侧添加 ready API，在 Orion 侧 mount 后等待 ready 再启动 buck2。

### 修改 1: Scorpio — 添加 MountLifecycle::Ready 状态

**文件：`scorpio/src/daemon/antares.rs`**

在 `MountLifecycle` 枚举中添加 `Ready` 状态：

```rust
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub enum MountLifecycle {
    Provisioning,
    Mounted,
    Ready,       // ← 新增：目录树深度预加载完成
    Unmounting,
    Unmounted,
    Failed { reason: String },
}
```

**为什么这样做**：将 mount 和 ready 分为两个阶段。`Mounted` 表示 FUSE 挂载成功可以读写，`Ready` 表示目录树缓存已充分预热，适合重 I/O 工作负载。这样 orion 可以在 `Ready` 之前执行轻量操作，`Ready` 之后再启动 buck2。

### 修改 2: Scorpio — 添加 `/mounts/{mount_id}/ready` API

**文件：`scorpio/src/daemon/antares.rs`**

在 `AntaresService` trait 添加方法：

```rust
async fn check_mount_ready(&self, mount_id: Uuid)
    -> Result<MountReadyResponse, ServiceError>;
```

响应结构：

```rust
pub struct MountReadyResponse {
    pub mount_id: Uuid,
    pub ready: bool,           // true 当 state == Ready
    pub state: MountLifecycle, // 当前状态
}
```

路由：`GET /antares/mounts/{mount_id}/ready`

**为什么这样做**：提供一个轻量级的轮询端点，orion 可以每 500ms 查询一次而不会给 scorpio 造成压力。返回 `ready: bool` 便于客户端判断。

### 修改 3: Scorpio — create_mount 后台深度预加载

**文件：`scorpio/src/daemon/antares.rs`**

在 `create_mount` 返回 `MountCreated` 之前，spawn 一个后台 tokio task：

```rust
tokio::spawn(async move {
    let walk_result = tokio::task::spawn_blocking(move || {
        deep_preload_walk(&mountpoint)  // 同步遍历目录树
    }).await;

    // 遍历完成后将状态从 Mounted → Ready
    if let Some(entry) = mounts.write().await.get_mut(&mount_id) {
        if matches!(entry.state, MountLifecycle::Mounted) {
            entry.state = MountLifecycle::Ready;
        }
    }
});
```

`deep_preload_walk` 函数对整个挂载点做深度优先遍历，对每个 entry 调用 `metadata()` 触发 FUSE `getattr`，从而预热内核 entry/attr 缓存（TTL = 60s）。

**为什么这样做**：
1. **从挂载点侧遍历**（而非 Dicfuse 内部）：这样走的是完整的 OverlayFs → Dicfuse 路径，确保内核 FUSE 缓存被实际预热。Dicfuse 内部的 `import_arc` 只预热了应用层 DictionaryStore 缓存，内核缓存仍是冷的。
2. **spawn_blocking**：目录遍历是同步阻塞 I/O，不应阻塞 tokio 运行时。
3. **不阻塞 create_mount 返回**：mount API 立即返回 mountpoint，后台预热异步进行。

### 修改 4: Orion — mount 后轮询等待 Ready

**文件：`orion/src/buck_controller.rs`**

新增 `wait_for_mount_ready` 函数：

```rust
pub async fn wait_for_mount_ready(mount_id: &str) -> Result<(), ...> {
    let deadline = Instant::now() + Duration::from_secs(600); // 10分钟超时
    loop {
        if Instant::now() >= deadline {
            return Ok(()); // 超时不失败，继续 best-effort
        }
        // GET /antares/mounts/{mount_id}/ready
        if response.ready { return Ok(()); }
        sleep(500ms).await;
    }
}
```

在 `build()` 函数中，`mount_antares_fs` 之后、`get_build_targets` 之前调用：

```rust
// 新增：等待两个 mount 都 ready
wait_for_mount_ready(&mount_id).await?;
wait_for_mount_ready(&mount_id_old_repo).await?;
```

**为什么这样做**：
1. **在 mount 后、buck2 前** 等待确保 buck2 的 statx 风暴命中热缓存。
2. **10 分钟超时** 足够大型 monorepo 预加载。超时后不失败，而是降级为之前的冷缓存行为。
3. **轮询而非长连接** 避免了 HTTP 超时问题，且每 500ms 一次请求对 scorpio 几乎没有压力。

### 修改 5: Orion — preheat 改为轻量级

**文件：`orion/src/buck_controller.rs`**

原来的 `preheat` 调用 `ls -lR` 做全量遍历。改为复用 `preheat_shallow` 做可控深度的预热，作为 scorpio 深度预加载的补充：

```rust
fn preheat(repo_path: &Path) -> anyhow::Result<()> {
    let depth = preheat_shallow_depth();
    preheat_shallow(repo_path, depth)?;
    Ok(())
}
```

**为什么这样做**：`ls -lR` 无法控制深度和错误处理。改用 `preheat_shallow` 作为二次补充，主要缓存预热由 scorpio 侧的 `deep_preload_walk` 完成。

---

## 修复前后的流程对比

### 修复前

```
mount_antares_fs()
  → scorpio: FUSE mount OK (import_arc 后台还在跑)
  → orion: preheat (ls -lR) → 冷缓存，每个目录触发网络请求 → 慢
  → orion: buck2 build → statx 风暴 → FUSE daemon 忙于网络请求 → buck2 超时
  → buck2 daemon 被杀 → passthrough file handle 失效 → ESTALE
  → 恶性循环
```

### 修复后

```
mount_antares_fs()
  → scorpio: FUSE mount OK
  → scorpio: 后台 deep_preload_walk 遍历整个挂载点
    → 每个 entry 的 metadata() → FUSE getattr → 内核缓存预热 (TTL=60s)
    → 完成后 state: Mounted → Ready
  → orion: wait_for_mount_ready() 轮询直到 Ready
  → orion: preheat_shallow (轻量补充)
  → orion: buck2 build → statx 全部命中内核缓存 → 秒级完成
```

---

## 架构图

```
┌─────────────────────────────────────────────────────────────────┐
│                    buck2 daemon (用户进程)                        │
│         statx() × N 个文件/目录                                   │
└──────────────────────┬──────────────────────────────────────────┘
                       │ syscall
                       ▼
┌──────────────────────────────────────────────────────────────────┐
│                Linux Kernel FUSE 模块                             │
│  ┌─────────────────────────────────────────────────────────┐     │
│  │ entry cache (TTL = reply_ttl = 60s for Antares)         │     │
│  │ attr cache  (TTL = reply_ttl = 60s for Antares)         │     │
│  └──────────────────────┬──────────────────────────────────┘     │
│                         │ 缓存 miss 时                            │
└─────────────────────────┼────────────────────────────────────────┘
                          │ /dev/fuse
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│              rfuse3 Session (scorpio 进程内)                       │
│                         │                                        │
│   ┌─────────────────────▼──────────────────────────────┐         │
│   │            OverlayFs (libfuse-fs)                   │         │
│   │  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │         │
│   │  │ upper    │  │ CL layer │  │ lower (Dicfuse)   │  │         │
│   │  │passthru  │  │passthru  │  │  ┌──────────────┐ │  │         │
│   │  │ (rw)     │  │ (rw/CL)  │  │  │DictionaryStore│ │  │         │
│   │  └────┬─────┘  └──────────┘  │  │  ┌─────────┐ │ │  │         │
│   │       │                      │  │  │dirs cache│ │ │  │         │
│   │       │open_by_handle_at     │  │  │(DashMap) │ │ │  │         │
│   │       │→ ESTALE if daemon    │  │  │TTL=120s  │ │ │  │         │
│   │       │  was restarted       │  │  └─────────┘ │ │  │         │
│   │       │                      │  │      │       │ │  │         │
│   │       ▼                      │  │      │ miss  │ │  │         │
│   │  local filesystem            │  │      ▼       │ │  │         │
│   │                              │  │ fetch_dir()  │ │  │         │
│   │                              │  │ HTTP GET     │ │  │         │
│   │                              │  └──────┬───────┘ │  │         │
│   │                              │         │         │  │         │
│   │                              └─────────┼─────────┘  │         │
│   └────────────────────────────────────────┼────────────┘         │
│                                            │                     │
└────────────────────────────────────────────┼─────────────────────┘
                                             │ HTTP
                                             ▼
                                    ┌─────────────────┐
                                    │  Mega Server     │
                                    │  /api/v1/tree/   │
                                    │  content-hash    │
                                    └─────────────────┘
```

---

## 关键代码路径索引

| 功能 | 文件 | 行号 | 说明 |
|------|------|------|------|
| FUSE lookup 实现 | `scorpio/src/dicfuse/async_io.rs` | 68-184 | 非阻塞 lookup，20ms/200ms 超时 |
| FUSE getattr 实现 | `scorpio/src/dicfuse/async_io.rs` | 35-48 | 获取文件属性 |
| FUSE readdirplus 实现 | `scorpio/src/dicfuse/async_io.rs` | 611-731 | 目录列表 + 属性 |
| Reply TTL 计算 | `scorpio/src/dicfuse/mod.rs` | 262-270 | 区分普通/Antares 挂载 |
| 目录懒加载 | `scorpio/src/dicfuse/store.rs` | 1131-1227 | ensure_dir_loaded() |
| 目录刷新检查 | `scorpio/src/dicfuse/store.rs` | 1013-1019 | dir_refresh_needed() |
| 远程目录获取 | `scorpio/src/dicfuse/store.rs` | 463-520 | fetch_dir() HTTP 请求 |
| 后台目录预加载 | `scorpio/src/dicfuse/store.rs` | 2192-2390 | import_arc + load_dir_depth |
| Mount options | `scorpio/src/server/mod.rs` | 62-70 | write_back cache |
| Antares overlay 构建 | `scorpio/src/antares/fuse.rs` | 44-82 | build_overlay() |
| OverlayFs mount | `scorpio/src/antares/fuse.rs` | 85-113 | mount() |
| Passthrough file handle | `libfuse-fs/passthrough/file_handle.rs` | 296-315 | open_by_handle_at ESTALE |
| Buck2 preheat | `orion/src/buck_controller.rs` | 421-433 | ls -lR 全量预热 |
| Buck2 preheat_shallow | `orion/src/buck_controller.rs` | 436-481 | 浅层预热 |
| Buck2 build 主流程 | `orion/src/buck_controller.rs` | 730-940 | mount → preheat → build |
| Buck2 isolation dir | `orion/src/buck_controller.rs` | 558-565 | SHA256 隔离目录名 |
| 配置默认值 | `scorpio/src/util/config.rs` | 17-61 | TTL/超时/缓存默认值 |
| Scorpio 配置文件 | `scorpio/scorpio.toml` | — | 运行时配置 |

---

## 配置参数参考

### scorpio.toml 中的 Antares 相关配置

```toml
# ===== 普通 Dicfuse 挂载 =====
dicfuse_dir_sync_ttl_secs = "5"       # 目录刷新间隔（应用层）
dicfuse_reply_ttl_secs = "2"          # 内核 entry/attr 缓存 TTL

# ===== Antares 构建挂载 =====
antares_dicfuse_dir_sync_ttl_secs = "120"  # Antares 目录刷新间隔（2分钟）
antares_dicfuse_reply_ttl_secs = "60"      # Antares 内核缓存 TTL（1分钟）
antares_load_dir_depth = "3"               # Antares 预加载目录深度
antares_dicfuse_stat_mode = "fast"         # Antares stat 模式（不走网络获取大小）
```

### 环境变量

```bash
ORION_PREHEAT_SHALLOW_DEPTH=3   # 浅层预热深度
MEGA_BUILD__ORION_PREHEAT_SHALLOW_DEPTH=3  # 同上（build config 优先级）
BUCK_PROJECT_ROOT=/path/to/repo  # Buck2 项目根目录
```

---

## 调试方法

### 1. 使用 strace 观察系统调用

```bash
# 观察 buck2 daemon 的 statx 调用
strace -e statx -p <buck2_daemon_pid> -c  # 统计模式
strace -e statx -p <buck2_daemon_pid> -t  # 带时间戳

# 观察 scorpio daemon 的网络 IO
strace -e sendto,recvfrom -p <scorpio_pid>
```

### 2. 查看 FUSE 内核缓存统计

```bash
# 查看 FUSE 挂载信息
cat /proc/mounts | grep fuse
mount | grep fuse

# 查看 FUSE 连接详情
ls -la /sys/fs/fuse/connections/
cat /sys/fs/fuse/connections/<id>/waiting  # 等待中的请求数
```

### 3. Dicfuse 日志

```bash
# 开启 scorpio debug 日志
RUST_LOG=scorpio=debug,libfuse_fs=debug cargo run -p scorpio

# 过滤关键日志
# 目录加载：
grep "ensure_dir_loaded\|load_dir_depth\|import_arc" <log>
# 网络请求：
grep "fetch_dir\|fetch_file" <log>
# TTL/缓存：
grep "dir_refresh_needed\|reply_ttl" <log>
```

### 4. 手动验证预加载

```bash
# 在 mount 完成后手动 ls 预热，然后再执行 buck2
time ls -lR /path/to/mount > /dev/null 2>&1
# 如果这个命令本身就很慢，说明 import_arc 还没完成

# 检查 import 是否完成（看 marker 文件）
ls -la /tmp/megadir-*/store/dicfuse/*/import_done 2>/dev/null
```

---

## 2026-02-25 实际调试记录（本次执行）

### A. 依赖版本对齐检查（libfuse-fs）

执行：

```bash
cargo search libfuse-fs --limit 5
```

结果：

```text
libfuse-fs = "0.1.11"
```

同时检查：

- `scorpio/Cargo.toml`：`libfuse-fs = { version = "0.1.11" }`
- `Cargo.lock`：`libfuse-fs 0.1.11`

结论：当前仓库已经是 crates.io 最新版本，无需升级。

### B. 调试环境准备

为了避免影响线上 `:2725` 进程，本次在隔离配置下启动新进程：

- 配置文件：`/tmp/scorpio-debug.toml`
- 目录根：`/tmp/scorpio-megadir-debug/...`
- 端口：`127.0.0.1:2726`

启动命令：

```bash
RUST_LOG=scorpio=info,libfuse_fs=info \
cargo run -p scorpio --bin scorpio -- \
  --config-path /tmp/scorpio-debug.toml \
  --http-addr 127.0.0.1:2726
```

健康检查：

```bash
curl http://127.0.0.1:2726/antares/health
```

返回：`{"status":"healthy","mount_count":0,...}`

### C. Ready 机制验证（核心）

#### 实验 1：不等待 Ready，立即全量扫描（冷启动）

步骤：

1. `POST /antares/mounts` 创建 mount（path=`/`）
2. 不等待 ready，立即执行：
   `time ls -lR <mountpoint> > /dev/null`
3. 扫描结束后查询：
   `GET /antares/mounts/{mount_id}/ready`

结果：

- `COLD_SCAN_SECS = 108.396s`
- ready 查询结果：`{"ready": false, "state": "Mounted"}`

说明：冷缓存下 metadata 风暴会显著变慢，且此时挂载尚未进入 `Ready`。

#### 实验 2：先等待 Ready，再做同样扫描（热启动）

步骤：

1. 创建新 mount
2. 每 500ms 轮询 `GET /antares/mounts/{mount_id}/ready`，直到 `ready=true`
3. Ready 后执行同样的 `ls -lR`

结果：

- `WAIT_READY_SECS = 130.466s`
- Ready 状态：`{"ready": true, "state": "Ready"}`
- `WARM_SCAN_SECS = 2.610s`

对比结论（同样工作负载）：

| 场景 | 扫描耗时 | 状态 |
|------|---------:|------|
| 冷启动（不等 Ready） | 108.396s | Mounted / ready=false |
| 热启动（等 Ready） | 2.610s | Ready / ready=true |

可见 Ready 机制能显著降低 metadata storm 延迟。

### D. Buck2 直接复现实验

在 mount 根目录执行：

```bash
buck2 --isolation-dir <id> targets //...
```

分别在“冷启动立即跑”和“Ready 后跑”执行，结果一致，快速失败：

```text
Command failed: Error initializing DaemonStateData
Caused by:
  0: disk I/O error
  1: Error code 3850: I/O error in the advisory file locking layer
```

结论：

- 本机环境中，buck2 被 advisory lock I/O 错误提前阻断，未进入之前线上日志中的“长时间 statx + daemon 超时 + ESTALE”阶段。
- 因此本次**未完整复现**旧问题链路（timeout + stale file handle），但已复现“冷缓存 metadata 风暴显著慢”的关键前提，并验证了新 `Ready` 机制的收益。

### E. Scorpio 日志证据

日志文件：`/tmp/scorpio-debug-2726.log`

关键行：

- `antares svc: mount transitioned to Ready mount_id=...`
- 共 4 个测试 mount 均出现 Ready 迁移日志
- 本次日志中未出现 `open_by_handle_at failed` / `Stale file handle`

### F. 清理动作

调试结束后已执行：

1. 删除所有临时 mount（`DELETE /antares/mounts/{mount_id}`）
2. 停止调试进程（`:2726`）

避免对线上实例和本机 FUSE 资源造成残留影响。

---

## `Error initializing DaemonStateData` 的根因与修复

### 现象

执行 `buck2 --isolation-dir ... targets //...` 时快速失败：

```text
Command failed: Error initializing DaemonStateData
Caused by:
  0: disk I/O error
  1: Error code 3850: I/O error in the advisory file locking layer
```

### 精确根因（已定位）

通过 `strace` 抓到失败系统调用：

```text
openat(..., "<mount>/buck-out/<iso>/cache/materializer_state/db.sqlite", ...) = 19
fcntl(19, F_SETLK, ...) = -1 ENOSYS (Function not implemented)
```

并通过最小化测试确认：

- 在 Antares mount 内文件上 `flock()` **可用**
- 但 `lockf()/fcntl(F_SETLK)` 返回 `ENOSYS(38)`

原因是当前 `libfuse-fs` 0.1.11 的 `overlayfs/passthrough` 对 `setlk/getlk` 返回 `ENOSYS`，Buck2 materializer state 的 sqlite DB 依赖 advisory lock，初始化因此失败。

### 实际可用修复（已落地到 Orion）

**策略：把 `<mount>/buck-out` 重定向到本地磁盘目录**（例如 `/tmp/mega-buck-out/...`），避开 FUSE 锁实现限制。

已在 `orion/src/buck_controller.rs` 增加：

1. `prepare_local_buck_out(mount_root)`  
   - 为每个 mount 计算唯一本地目录  
   - 创建 `<mount>/buck-out -> /tmp/mega-buck-out/...` 的符号链接
2. 在每次 `mount_antares_fs` 成功后、执行任何 buck2 命令前调用该函数。
3. 最终 build 阶段再次确保链接存在。

验证结果：

- 未重定向 buck-out：报 `DaemonStateData` advisory lock 错误。
- 重定向 buck-out 后：Buck2 daemon 能正常启动并连接，后续进入真实解析阶段（报错变为业务配置错误，而非锁错误）。

### 手动临时绕过命令（不改代码时）

在 mount 成功后手动执行：

```bash
MOUNT=/tmp/scorpio-megadir/antares/mnt/<mount_id>
LOCAL=/tmp/buck2-out-local-<mount_id>
mkdir -p "$LOCAL"
cd "$MOUNT"
rm -rf buck-out
ln -s "$LOCAL" buck-out
buck2 --isolation-dir debug targets //...
```

> 注意：这是临时方案。正式流程应由 Orion 自动处理，避免人工遗漏。

---

## 2026-02-25 第二轮调试（已还原 buck-out 本地化改动）

按需求已先还原 `orion/src/buck_controller.rs` 中 `buck-out` 本地重定向逻辑，然后继续针对历史日志中的：

```text
open_by_handle_at failed ... Stale file handle
```

进行专项复现。

### 实验设计

使用独立 Scorpio 实例（`127.0.0.1:2727`）并打开 `libfuse_fs::passthrough::file_handle` 日志，执行 10 轮高压竞态：

1. `POST /antares/mounts` 挂载
2. 并发启动：
   - 深层 `find + stat` 元数据风暴
   - `buck2 --isolation-dir ... targets //...`
3. 在扫描过程中立刻 `DELETE /antares/mounts/{id}` 强制 unmount

### 结果

- 10 轮均成功 mount/unmount（API 视角均返回 `Unmounted`）
- **未出现** `open_by_handle_at failed` / `StaleNetworkFileHandle` / `ESTALE`
- 出现大量：
  - `deep_preload_walk: read_dir failed ... ENOENT`
  - `fuse task did not complete within 5s ... continuing anyway`

### 新发现（可疑竞态）

`create_mount` 后台启动的 `deep_preload_walk` 在 mount 已经被删除时仍继续扫描，导致大量 `ENOENT` 和卸载耗时告警。  
这虽然不是 `ESTALE`，但说明存在明显时序竞态：

```
mount created -> deep_preload_walk spawn
    \-> request triggers unmount quickly
        \-> mountpoint removed
            \-> preload walker still traverses old path -> ENOENT storm
```

### 当前结论

1. 在当前代码和当前环境下，历史中的 `open_by_handle_at stale file handle` **未复现**。
2. 但发现了另一个稳定可复现的问题：`deep_preload_walk` 与 unmount 并发竞态（高频 ENOENT + fuse task timeout）。
3. 下一步建议：
   - 给每个 mount 引入可取消的 preload task（Unmounting 时 cancel）
   - 或在 walker 循环内检查 mount 状态，非 `Mounted/Ready` 立即退出
   - 避免后台任务访问已销毁 mountpoint

---

## 第三轮：修复 POSIX advisory lock (fcntl F_SETLK/F_GETLK) 缺失导致 Buck2 初始化失败

### 问题描述

Buck2 daemon 启动时需要对 `materializer_state/db.sqlite` 执行 POSIX advisory lock
（`fcntl(F_SETLK, F_WRLCK)`），而我们的 FUSE 栈此前未实现这套锁，返回 `ENOSYS`，导致：

```
Error initializing DaemonStateData
disk I/O error
I/O error in the advisory file locking layer
```

### 根因分析

| 锁类型 | 系统调用 | 修复前状态 | Buck2 需要? |
|--------|---------|-----------|------------|
| `flock()` | `flock(fd, LOCK_EX)` | ✅ 可用（内核自动处理） | ❌ |
| `fcntl(F_SETLK)` / `lockf()` | `fcntl(fd, F_SETLK, &flock)` | ❌ 返回 ENOSYS | ✅ sqlite WAL |
| `fcntl(F_GETLK)` | `fcntl(fd, F_GETLK, &flock)` | ❌ 返回 ENOSYS | ✅ sqlite WAL |

FUSE 中 `flock()` 由内核自动处理，不需要 userspace daemon 参与。
但 `fcntl(F_SETLK/F_GETLK)` 是 POSIX advisory lock，需要 FUSE daemon 显式实现
`FUSE_GETLK`/`FUSE_SETLK`/`FUSE_SETLKW` 操作。

### 修复方案

在 `libfuse-fs` 和 `scorpio` 的 4 层全部实现了 `getlk/setlk`：

#### 1. `libfuse-fs` passthrough 层（底层真正的 fcntl 调用）

**文件**: `libfuse-fs/src/passthrough/async_io.rs`

```rust
async fn getlk(&self, _req, inode, fh, _lock_owner, start, end, type, pid) -> Result<ReplyLock> {
    let data = self.get_data(fh, inode, libc::O_RDONLY).await?;
    let raw_fd = data.borrow_fd().as_raw_fd();
    let mut flock = libc::flock { l_type, l_whence: SEEK_SET, l_start, l_len, l_pid };
    let ret = unsafe { libc::fcntl(raw_fd, libc::F_GETLK, &mut flock) };
    // ... 转换回 ReplyLock
}

async fn setlk(&self, _req, inode, fh, _lock_owner, start, end, type, _pid, block) -> Result<()> {
    let data = self.get_data(fh, inode, libc::O_RDONLY).await?;
    let raw_fd = data.borrow_fd().as_raw_fd();
    let flock = libc::flock { l_type, l_whence: SEEK_SET, l_start, l_len, l_pid: 0 };
    let cmd = if block { F_SETLKW } else { F_SETLK };
    let ret = unsafe { libc::fcntl(raw_fd, cmd, &flock) };
    // ...
}
```

#### 2. `libfuse-fs` overlayfs/unionfs 层（路由到底层 layer）

**文件**: `libfuse-fs/src/overlayfs/async_io.rs`, `libfuse-fs/src/unionfs/async_io.rs`

```rust
async fn getlk(&self, req, inode, fh, lock_owner, start, end, type, pid) -> Result<ReplyLock> {
    let data = self.get_data(req, Some(fh), inode, 0).await?;
    match data.real_handle {
        None => Err(ENOENT),
        Some(ref hd) => hd.layer.getlk(req, hd.inode, hd.handle, lock_owner, start, end, type, pid).await,
    }
}
// setlk 同理
```

#### 3. `scorpio` dicfuse 层（只读文件系统）

**文件**: `scorpio/src/dicfuse/async_io.rs`

- `getlk` → 返回 `F_UNLCK`（无锁持有）
- `setlk` → 返回 `EROFS`（只读文件系统拒绝加锁）

#### 4. `scorpio` MemUpperLayer（测试用内存层）

**文件**: `scorpio/src/antares/fuse.rs`

- `getlk` → 返回 `F_UNLCK`
- `setlk` → 返回 `Ok(())`（接受但不真正执行锁定）

### 构建配置变更

| 文件 | 变更 |
|------|------|
| `Cargo.toml` (workspace) | 添加 `[patch.crates-io]` 指向本地 `libfuse-fs` |
| `scorpio/Cargo.toml` | rfuse3 features 添加 `"file-lock"` |

### 复现方法

编译 C 测试程序模拟 Buck2 的 sqlite 锁行为：

```c
// test_fcntl_lock.c
#include <fcntl.h>
#include <stdio.h>
int main(int argc, char *argv[]) {
    int fd = open(argv[1], O_RDWR | O_CREAT, 0644);
    struct flock fl = { .l_type = F_WRLCK, .l_whence = SEEK_SET };
    if (fcntl(fd, F_SETLK, &fl) < 0) { perror("F_SETLK"); return 2; }
    printf("F_SETLK OK\n");
    if (fcntl(fd, F_GETLK, &fl) < 0) { perror("F_GETLK"); return 3; }
    printf("F_GETLK OK\n");
    fl.l_type = F_UNLCK;
    fcntl(fd, F_SETLK, &fl);
    printf("ALL PASSED\n");
}
```

```bash
gcc -o test_fcntl_lock test_fcntl_lock.c

# 修复前（在 FUSE overlay 挂载点）:
./test_fcntl_lock /mnt/antares/<mount-id>/test_file
# => F_SETLK FAILED: Function not implemented (errno=38)

# 修复后:
./test_fcntl_lock /mnt/antares/<mount-id>/test_file
# => F_SETLK OK
# => F_GETLK OK (type=2, pid=0)
# => ALL PASSED
```

### 验证结果

在修复后的 scorpio 上创建 Antares overlay mount 并运行测试：

```
[OK] opened .../6f8d59bf-.../test_lock_file (fd=5)
[..] fcntl(F_SETLK, F_WRLCK) ... OK
[..] fcntl(F_GETLK, F_WRLCK) ... OK (type=2, pid=0)
[..] fcntl(F_SETLK, F_UNLCK) ... OK

=== ALL TESTS PASSED ===
```

### 与之前问题的关系

| 问题 | 根因 | 本次修复是否解决 |
|------|------|----------------|
| mount 失败（删 ANTARES_FUSE_CACHE_MOUNT_OPTIONS 修好） | 不兼容的 libfuse2 mount options | ❌ 无关（已在之前修复） |
| Buck2 DaemonStateData 初始化失败 | fcntl advisory lock 返回 ENOSYS | ✅ 本次修复 |
| statx storm + daemon timeout | 目录预热不足 | ❌ 已在 deep_preload_walk + Ready 状态修复 |
| open_by_handle_at stale file handle | 文件删除后内核对旧 inode 发起 getattr，旧 handle 已失效 | ✅ 已定位并修复（见下方） |

---

## 问题 4：open_by_handle_at Stale File Handle (ESTALE)

### 根因分析

**完整调用链（经 strace 验证）：**

1. 用户执行 `rm file.txt` → 内核发送 `FUSE_UNLINK`
2. Passthrough 层处理 unlink：
   - `open_by_handle_at(mount_fd, parent_handle, O_PATH)` → 打开父目录 ✅
   - `statx(parent_fd, "file.txt")` → 获取文件 btime 用于缓存失效 ✅
   - `unlinkat(parent_fd, "file.txt")` → 删除底层文件 ✅
   - `handle_cache.invalidate(key)` → 失效文件句柄缓存 ✅
3. Passthrough unlink 返回成功
4. **内核在 unlink 完成后，对旧 inode 发起 `FUSE_GETATTR`**（更新 nlink 等元数据）
5. Passthrough `getattr` → `inode_data.get_file()` → `open_by_handle_at(mount_fd, old_file_handle, O_PATH)` → **ESTALE**

**strace 证据：**

```
# 同一线程，同一时刻：
[pid 1366326] unlinkat(15, "detail_test.txt", 0) = 0          ← 删除成功
[pid 1366326] open_by_handle_at(53, {...file_handle...}, O_PATH) = -1 ESTALE  ← 旧 handle 已失效
```

**关键洞察：**
- `open_by_handle_at` 通过 `name_to_handle_at` 获取的 file handle 与真实 inode 绑定
- 文件被删除后，底层 inode 消失，旧 handle 变为 stale
- 内核在 FUSE_UNLINK 返回后仍可能对旧 inode 发起 FUSE_GETATTR
- 这是 **良性错误**（benign）：所有用户操作均成功（验证 300 次删除+重建，0 个用户可见错误）

### 复现方法

```bash
# 在 FUSE overlay 挂载点上：
mkdir -p $MNTPT/buck-out/test
for i in $(seq 1 50); do echo "data" > $MNTPT/buck-out/test/f_$i.txt; done
for i in $(seq 1 50); do rm -f $MNTPT/buck-out/test/f_$i.txt; done
# 查看 scorpio 日志：每删一个文件产生一个 ESTALE
grep "stale" /path/to/scorpio.log | wc -l  # → 50
```

### 修复内容

**文件：`libfuse-fs/src/passthrough/file_handle.rs` — `OpenableFileHandle::open()`**

修复前：
```rust
let e = io::Error::last_os_error();
error!("open_by_handle_at failed error {e:?}");  // ERROR 级别
Err(e)  // 返回 ESTALE
```

修复后：
```rust
let e = io::Error::last_os_error();
if e.raw_os_error() == Some(libc::ESTALE) {
    // ESTALE 在文件删除后是预期行为：内核可能在发送 forget 之前
    // 对旧 inode 发起 getattr。转为 ENOENT 让调用方正确处理。
    warn!("open_by_handle_at: stale handle (file likely deleted), returning ENOENT");
    Err(io::Error::from_raw_os_error(libc::ENOENT))
} else {
    error!("open_by_handle_at failed error {e:?}");
    Err(e)
}
```

**改动说明：**
1. **日志降级**：ESTALE 从 `error!` 降为 `warn!`，避免在正常文件删除操作时大量产生误导性 ERROR 日志
2. **错误转译**：ESTALE → ENOENT，语义更准确（"文件不存在" vs "网络文件句柄失效"）
3. **行为保持**：错误仍然传播到上层，不会掩盖真正的问题

### 验证结果

```
# 修复前：
ERROR lines: 50                    # 每次删除产生一个 ERROR
open_by_handle_at failed error: 50  # 误导性的 ERROR 日志

# 修复后：
ERROR lines: 0                      # 没有 ERROR
WARN lines with stale: 300          # 改为 WARN（300 = 50 files × 5 cycles + 50 initial）
User-visible errors: 0/50           # 所有用户操作成功
Rapid 5-round cycle errors: 0       # 压力测试全部通过
```

### 与 Buck2 原始问题的关系

原始日志中的 ESTALE 错误：
```
[2026-02-24T03:22:48Z ERROR] open_by_handle_at failed error Os { code: 116... "Stale file handle" }
[2026-02-24T03:24:46Z] Buck2 daemon pid 1232056 has exited
```

**结论：ESTALE 不是 Buck2 崩溃的直接原因。** Buck2 崩溃链为：
1. ❌ `fcntl(F_SETLK)` 返回 ENOSYS → SQLite 无法加锁 → DaemonStateData 初始化失败（已修复）
2. ❌ statx storm → daemon 超时 → 被客户端杀死（已通过 deep_preload + Ready 状态修复）
3. ⚠️ ESTALE 是并发的良性错误，不影响功能，但 ERROR 级别日志会混淆调试（已修复日志级别）

---

## 问题 5：`build_cl()` unmount/remount 导致并发进程崩溃分析

### 背景：为什么同一个 monorepo 会有多个 FUSE mount？

虽然 Antares 挂载的始终是同一个 monorepo（`path = "/"`），但 **每次构建任务都会创建独立的 FUSE mount**。这是因为：

1. **`create_mount` 每次生成新 UUID**（`scorpio/src/daemon/antares.rs:1330`）：

```rust
let mount_id = Uuid::new_v4();
let mountpoint_str = format!("{}/{}", mount_root, id_str);
```

2. **一次 `build()` 调用创建 2 个 mount**（`orion/src/buck_controller.rs:881`）：

```text
build(task="CI-123", repo="/project/foo", cl="CL-42")

  ① mount_antares_fs(path="/", cl=None)          ← old_repo（基线快照）
     → mountpoint = /var/lib/antares/mounts/<uuid-A>

  ② mount_antares_fs(path="/", cl="CL-42")       ← new_repo（CL 变更快照）
     → mountpoint = /var/lib/antares/mounts/<uuid-B>
```

3. **`--isolation-dir` 因此必须保留**：每个 mount UUID 不同 → `project_root` 路径不同。不用 `--isolation-dir` 会导致 Buck2 daemon 交叉污染（旧 daemon 的内部路径指向已卸载的旧 mount → ESTALE/ENOENT）。详见 `buck2_isolation_dir()` 函数文档。

### `build_cl` API 的 unmount/remount 流程

`POST /antares/mounts/{mount_id}/cl` 的实现（`scorpio/src/daemon/antares.rs:1710-1860`）做了：

```text
build_cl(mount_id, cl_link):
  1. 取出旧 AntaresFuse 实例 (old_fuse)
  2. state → Unmounting
  3. old_fuse.unmount()                    ← 卸载旧 FUSE！
  4. build_cl_layer(path, cl_link, cl_dir) ← 从远程下载 CL 文件到 cl_dir
  5. new_fuse = AntaresFuse::new(... cl_dir)
  6. new_fuse.mount()                      ← 重新挂载（包含 CL 层）
  7. state → Mounted
```

**关键问题**：步骤 3 到步骤 6 之间存在一个**挂载间隙窗口**，此时 mountpoint 变成空目录。

### 对并发进程的影响

```text
时间线：
  t0  mount 创建成功，Buck2 daemon 启动，持有 fd
  t1  build_cl 被调用
  t2  old_fuse.unmount()  ← FUSE 卸载！
      ├── mountpoint 变成空目录
      ├── Buck2 daemon 持有的所有 fd 失效
      │   ├── 通过路径访问 → ENOENT（文件在空目录下不存在）
      │   └── 通过 file handle 访问 → ESTALE（旧 FUSE handle 已失效）
      └── Buck2 daemon 崩溃
  t3  build_cl_layer() 下载 CL 文件...
  t4  new_fuse.mount() ← 新 FUSE 重新挂载
      ├── 新 PassthroughFs 实例，handle_cache 全新
      └── 旧 daemon 的 fd/handle 仍然无效
  t5  Buck2 daemon 重启 → 在新 FUSE 上从头开始
```

### 复现实验（2026-02-25）

在本机 Scorpio 调试实例（`127.0.0.1:2726`）上执行：

```bash
# 1. 创建 mount，等待 Ready
# 2. 在 upper layer 创建文件：
mkdir -p $MNTPT/buck-out/test-estale
echo "data" > $MNTPT/buck-out/test-estale/file_1.txt

# 3. 启动后台读取器（模拟 Buck2 daemon 持有 fd）：
while true; do stat $MNTPT/buck-out/test-estale/file_1.txt; sleep 0.02; done &

# 4. 触发 build_cl：
curl -X POST .../mounts/$MOUNT_ID/cl -d '{"cl": "CL-TEST"}'
```

**结果**：

| 指标 | 值 |
|------|-----|
| 后台读取器死亡时间 | build_cl 发起后 ~200ms |
| 客户端看到的错误 | `ENOENT: No such file or directory`（空目录） |
| strace 捕获 | `statx(...) = -1 ENOENT` |
| scorpio 日志 ESTALE | 无（因为旧 FUSE 实例已销毁，不会有 `open_by_handle_at`） |
| build_cl 结果 | HTTP 500（CL 服务不可用），回退 remount 成功 |
| 回退 remount 后 | 文件可正常访问 |

**为什么客户端看到 ENOENT 而不是 ESTALE？**

- FUSE unmount 后，mountpoint 退化为**普通空目录**
- 客户端 `stat("空目录/buck-out/test-estale/file_1.txt")` 自然返回 `ENOENT`
- `ESTALE` 只出现在 **FUSE daemon 内部**（`open_by_handle_at` 用旧 handle 打开已删除的底层文件）

**在原始生产日志中**：

```
[03:22:48] open_by_handle_at failed error ... "Stale file handle"
[03:24:46] Buck2 daemon pid 1232056 has exited
```

这里 ESTALE 出现在 **daemon 侧**（scorpio 进程日志），说明在 unmount 过程中，旧 FUSE session 仍在处理 pending 的 `FUSE_GETATTR` 请求，此时 passthrough 层用旧的 `FileHandle` 去调用 `open_by_handle_at`，底层文件已经不存在 → `ESTALE`。

### 当前 Orion `build()` 是否受影响？

**当前 `build()` 流程不会触发此问题。** 原因：

1. Orion 的 `build()` **不调用** `build_cl` API
2. CL 层在 `mount_antares_fs()` → `create_mount()` 时**一次性构建好**（`antares.rs:1359-1371`）
3. `build()` 挂载两个独立 mount（old_repo 无 CL + new_repo 带 CL），不会对已有 mount 做 unmount/remount

```text
当前 build() 流程（安全）：
  mount_antares_fs(path="/", cl=None)     → UUID-A（old_repo，无 CL）
  mount_antares_fs(path="/", cl="CL-42")  → UUID-B（new_repo，CL 在 create_mount 时构建）
  wait_for_mount_ready(UUID-A)
  wait_for_mount_ready(UUID-B)
  buck2 targets on UUID-A + UUID-B
  buck2 build on UUID-B
  unmount UUID-A, UUID-B                  ← 构建完成后才卸载
```

### 但 `build_cl` API 仍然危险

`build_cl` API（`POST /mounts/{id}/cl`）设计上用于**对已有 mount 追加/更新 CL 层**，适用于长生命周期的 mount。如果在 mount 上有活跃进程（如 Buck2 daemon）时调用，会导致：

1. **进程崩溃**：所有持有 fd 的进程在 unmount 窗口内失败
2. **数据丢失**：upper layer 的未刷写数据可能丢失
3. **状态不一致**：Buck2 daemon 的 materializer_state 可能损坏

**建议**：

1. 调用 `build_cl` 前应先确保 mount 上无活跃进程（至少 `buck2 kill` 相关 daemon）
2. 或改用"创建新 mount + CL"代替"在已有 mount 上 build_cl"（当前 `build()` 已经这样做）
3. 长期方案：实现"热替换" CL 层（不需要 unmount/remount 整个 FUSE），但这需要 OverlayFs 层面支持动态增减 lower layers

### `--isolation-dir` 与 mount UUID 的关系

| 函数 | 文件 | 作用 |
|------|------|------|
| `buck2_isolation_dir(repo_path)` | `orion/src/buck_controller.rs:673` | 从 `SHA256(project_root)` 派生唯一名，确保不同 mount 用不同 daemon |
| `mount_antares_fs(job_id, path, cl)` | `orion/src/buck_controller.rs:206` | 每次调用 → `POST /antares/mounts` → 新 UUID |
| `build()` | `orion/src/buck_controller.rs:851` | 每次构建创建 2 个 mount（old_repo + new_repo），各有独立 UUID 和 isolation-dir |

`--isolation-dir` 通过 hash 映射确保：
- **同一 mount path → 同一 daemon**（重试时复用 daemon，避免重复启动）
- **不同 mount path → 不同 daemon**（避免交叉污染，防止 ESTALE）

---

## 附录：代码质量修复清单（2026-02-25）

### Orion → Scorpio API 依赖关系

| API 端点 | 用途 | Orion 调用位置 |
|----------|------|----------------|
| `POST /antares/mounts` | 创建 Antares FUSE mount | `mount_antares_fs()` |
| `GET /antares/mounts/{id}/ready` | 等待 deep preload 完成 | `wait_for_mount_ready()` |
| `DELETE /antares/mounts/{id}` | 卸载 mount | `unmount_antares_fs()` |
| `POST /api/fs/mount` | 旧式 mount（非 Antares） | `mount_fs_with_cl()` / `fs::mount_fs()` |
| `GET /api/fs/select/{id}` | 旧式 mount 轮询 | `mount_fs_with_cl()` / `fs::mount_fs()` |
| `POST /api/fs/unmount` | 旧式 unmount | `unmount_fs()` |

**Orion 不使用** `POST /mounts/{id}/cl`（build_cl）和 `DELETE /mounts/{id}/cl`（clear_cl）。

### 修复 1：`deep_preload_walk` 取消机制

**问题**：`create_mount` 后 spawn 的 `deep_preload_walk` 后台任务没有取消机制。`delete_mount` / `build_cl` / `clear_cl` 触发 unmount 时，walker 仍在访问已卸载的 mountpoint → ENOENT 日志风暴。

**修复**：
- `MountEntry` 新增 `preload_cancel: Arc<AtomicBool>` 字段
- `deep_preload_walk()` 每进入一个目录前检查 cancel 标志
- `delete_mount` / `build_cl` / `clear_cl` / `shutdown_cleanup` 在 unmount 前设置 `preload_cancel = true`

文件：`scorpio/src/daemon/antares.rs`

### 修复 2：`build_cl` / `clear_cl` remount 后重新触发 deep preload

**问题**：remount 后内核 FUSE 缓存全部清空，但状态直接设为 `Mounted` 而不重新 preload，导致 Orion poll `ready` 时拿到过期状态。

**修复**：
- `build_cl` / `clear_cl` 成功 remount 后重置 `preload_cancel`，spawn 新的 `deep_preload_walk` 后台任务
- preload 完成后自动转换为 `Ready` 状态

文件：`scorpio/src/daemon/antares.rs`

### 修复 3：FUSE negative lookup 缓存

**问题**：Buck2 反复 lookup 不存在的文件（`.buckconfig.local`、`.watchmanconfig` 等），每次穿透到 Dicfuse → 网络请求。

**修复**：
- `lookup()` 返回 `ReplyEntry { attr.ino = 0, ttl = 5s }` 代替 `Err(ENOENT)`
- FUSE 内核将缓存 negative dentry 5 秒，相同 lookup 不再穿透

文件：`scorpio/src/dicfuse/async_io.rs`

### 修复 4：`load_one_file` 中 `unwrap()` 替换为错误处理

**问题**：`response.bytes().await.unwrap()` 和 `client.get(url).send().await.unwrap()` 在网络故障时会 panic。

**修复**：替换为 `map_err(|e| io::Error::new(...))?`，错误会向上传播而不是 panic。

文件：`scorpio/src/dicfuse/mod.rs`

### 修复 5：符号链接跳过日志

**问题**：`load_files` 中 `TreeItemMode::Link` 被静默跳过，无任何日志。

**修复**：
- 添加 `tracing::debug!` 日志记录跳过的符号链接
- 改善注释说明符号链接需要 `readlink()` 支持（尚未实现）
- `eprintln!` 替换为 `tracing::warn!`

文件：`scorpio/src/dicfuse/mod.rs`

---

## 问题 6：Buck2 `set_cfg_constructor()` 解析失败

### 现象

在 FUSE 挂载上运行 `buck2 targets //project/libra/...` 报错：

```
Error parsing root//project/libra
evaluating Starlark PACKAGE file `root//project/libra`

Caused by:
    error: `set_cfg_constructor()` can only be called from the repository root `PACKAGE` file
      --> toolchains/buckal-bundles/config/set_cfg_constructor.bzl:10:9
```

### 根因分析

1. **`project/libra/PACKAGE` 调用了 `set_cfg_constructor()`**：

```python
# project/libra/PACKAGE (由 cargo buckal 自动生成)
load("@buckal//config:set_cfg_constructor.bzl", "set_cfg_constructor")
set_cfg_constructor(aliases = ALIASES)
```

2. **Buck2 内核级限制**：`native.set_cfg_constructor()` **只能从仓库根 `PACKAGE` 文件调用**，不论 cell 配置如何。

3. **Monorepo 根目录缺少 `PACKAGE` 文件**：

```bash
curl -s "http://git.gitmega.com/api/v1/tree?path=/" | python3 -c "..."
# Total items: 10
# .buckconfig, .buckroot, .cedar, .mega_cedar.json, doc, model, project, release, third-party, toolchains
# ❌ 没有 PACKAGE 文件
```

`set_cfg_constructor.bzl` 本身有 cell 守卫（检查 `project_root_cell == current_root_cell`），但 **Buck2 在 Starlark 层面就拒绝了非根 PACKAGE 文件的调用**，守卫代码根本没机会执行。

### 原因链

```
cargo buckal 生成 project/libra/PACKAGE（包含 set_cfg_constructor 调用）
  → 在独立仓库中工作正常（project/libra 就是根）
  → 被纳入 monorepo 后，project/libra 变成子目录
  → monorepo 根目录没有 PACKAGE 文件
  → set_cfg_constructor 从子目录 PACKAGE 调用 → Buck2 拒绝
```

**这是 monorepo 配置问题，不是 FUSE/Scorpio 问题。**

### 修复方法

在 monorepo 根目录创建 `PACKAGE` 文件，将 `set_cfg_constructor()` 调用提升到根级别：

**新建 `/PACKAGE`**（仓库根）：
```python
# Root PACKAGE file for monorepo
# set_cfg_constructor() MUST be called from the repository root PACKAGE file.

load("@prelude//cfg/modifier:set_cfg_modifiers.bzl", "set_cfg_modifiers")
load("@buckal//config:set_cfg_constructor.bzl", "set_cfg_constructor")

ALIASES = {
    "debug": "buckal//config/mode:debug",
    "release": "buckal//config/mode:release",
}
set_cfg_constructor(aliases = ALIASES)
```

**修改 `project/libra/PACKAGE`**（移除 set_cfg_constructor 调用）：
```python
# @generated by `cargo buckal`
# NOTE: set_cfg_constructor() moved to root PACKAGE file (Buck2 requirement).

load("@prelude//cfg/modifier:set_cfg_modifiers.bzl", "set_cfg_modifiers")

set_cfg_modifiers(
    cfg_modifiers = [
        "buckal//config/mode:debug",
    ],
)
```

### 验证结果（2026-02-25）

在 FUSE upper layer 中创建根 `PACKAGE` 并修改 `project/libra/PACKAGE` 后：

```bash
# 修复前：
buck2 targets //project/libra/...
# → Error: set_cfg_constructor() can only be called from root PACKAGE file

# 修复后：
buck2 targets //project/libra:
# → root//project/libra:libra
#   root//project/libra:libra-lib
#   root//project/libra:libra-manifest
#   root//project/libra:libra-vendor
# exit=0 ✅

buck2 build //project/libra:libra-vendor
# → BUILD SUCCEEDED ✅
```

递归 `//project/libra/...` 仍报错 `File not found: root//cxx/demo_cxx.bzl`（见下方问题 7）。

### 建议

1. **Monorepo 侧修复**：在 Mega 仓库根目录添加 `PACKAGE` 文件，包含 `set_cfg_constructor()` 调用
2. **子项目 PACKAGE 修复**：所有子项目的 `PACKAGE` 文件中移除 `set_cfg_constructor()`（保留 `set_cfg_modifiers`）
3. **Orion 侧 workaround**：在 `mount_antares_fs()` 成功后，如果 monorepo 根目录没有 `PACKAGE` 文件，可以自动在 upper layer 创建一个

---

## 问题 7：Dicfuse URL 编码 bug 导致含 `+` 的路径加载为空目录

### 现象

`third-party/rust/crates/zstd-sys/2.0.16+zstd.1.5.7/` 目录存在但内容为空（没有 BUCK 文件），导致 Buck2 报 `package does not exist`。

### 根因分析

Dicfuse 的 `fetch_dir()` / `fetch_tree()` / `fetch_get_dir_hash()` 使用字符串拼接构建 API URL：

```rust
// 修复前
let url = format!("{}/api/v1/tree/content-hash?path=/{}", base_url, clean_path);
// 当 clean_path = "third-party/rust/crates/zstd-sys/2.0.16+zstd.1.5.7" 时
// URL 中的 '+' 被 HTTP 服务器解释为空格 → 查询路径变成 "2.0.16 zstd.1.5.7" → 返回空结果
```

**验证**：
```bash
# + 不编码 → 0 items
curl -s "http://git.gitmega.com/api/v1/tree?path=/third-party/rust/crates/zstd-sys/2.0.16+zstd.1.5.7"
# → {"data": []}

# + 编码为 %2B → 1 item (BUCK)
curl -s "http://git.gitmega.com/api/v1/tree?path=/third-party/rust/crates/zstd-sys/2.0.16%2Bzstd.1.5.7"
# → {"data": [{"name": "BUCK", ...}]}
```

### 修复

**文件**：`scorpio/src/dicfuse/store.rs`

添加 `encode_api_path()` 辅助函数，对 `+` 和 `#` 等 URL 查询字符串保留字符进行 percent 编码：

```rust
fn encode_api_path(path: &str) -> String {
    let clean = path.trim_start_matches('/');
    let normalized = if clean.is_empty() { "/".to_string() } else { format!("/{clean}") };
    normalized.replace('+', "%2B").replace('#', "%23")
}
```

三个网络请求函数全部使用此函数：`fetch_tree()`、`fetch_dir()`、`fetch_get_dir_hash()`。

### 验证结果

```bash
# 修复前：
ls /mnt/.../zstd-sys/2.0.16+zstd.1.5.7/
# → (empty)

# 修复后：
ls /mnt/.../zstd-sys/2.0.16+zstd.1.5.7/
# → BUCK  ✅

buck2 targets //...   # 1248+ targets, exit=0 ✅
buck2 build //project/libra:libra  # 5322/5397 actions (98.6%)
# → 构建到 hyper-util 0.1.18 编译错误停止（monorepo 依赖兼容性问题，非 FUSE 问题）
```

---

## 问题 8：Monorepo 缺失 `cxx/` `rust/` toolchain 文件 + chrono 版本不匹配

### 现象

`project/libra/toolchains/BUCK` 引用了不存在的文件：

```starlark
load("@//cxx:demo_cxx.bzl", "system_demo_cxx_toolchain")  # → root//cxx/demo_cxx.bzl 不存在
load("@//rust:demo_rust.bzl", "system_demo_rust_toolchain") # → root//rust/demo_rust.bzl 不存在
```

多个 BUCK 文件引用 `chrono/0.4.43` 但 monorepo 只 vendor 了 `chrono/0.4.42`。

### 根因

`cargo buckal` 在独立仓库 `project/libra` 中生成的 BUCK 文件引用了 `@//cxx:...` 和 `@//rust:...`。在独立仓库中 `@//` 指向 `project/libra/`，但纳入 monorepo 后 `@//` 指向 monorepo 根目录，根目录没有这些文件。

**这些全部是 monorepo 配置/内容完整性问题，不是 FUSE/Scorpio 问题。**

### 临时修复（FUSE upper layer）

在 FUSE 可写层创建缺失文件：

```bash
# 1. cxx/demo_cxx.bzl — 包装 prelude 的 system_cxx_toolchain
mkdir -p $MNTPT/cxx && cat > $MNTPT/cxx/demo_cxx.bzl << 'EOF'
load("@prelude//toolchains:cxx.bzl", "system_cxx_toolchain")
def system_demo_cxx_toolchain(name = "cxx", **kwargs):
    system_cxx_toolchain(name = name, visibility = ["PUBLIC"], **kwargs)
EOF

# 2. rust/demo_rust.bzl — 包装 prelude 的 system_rust_toolchain
mkdir -p $MNTPT/rust && cat > $MNTPT/rust/demo_rust.bzl << 'EOF'
load("@prelude//toolchains:rust.bzl", "system_rust_toolchain")
def system_demo_rust_toolchain(name = "rust", **kwargs):
    system_rust_toolchain(name = name, visibility = ["PUBLIC"], **kwargs)
EOF

# 3. chrono/0.4.43 — 从 0.4.42 复制 BUCK 并更新版本号和 SHA256
mkdir -p $MNTPT/third-party/rust/crates/chrono/0.4.43
sed 's/0\.4\.42/0.4.43/g' .../chrono/0.4.42/BUCK > .../chrono/0.4.43/BUCK
# 手动更新 sha256 为实际 0.4.43 crate 的 hash
```

### 建议

这些问题应在 monorepo 仓库中永久修复：

1. 在 monorepo 根目录添加 `cxx/demo_cxx.bzl` 和 `rust/demo_rust.bzl`
2. 运行 `cargo buckal vendor` 更新 `third-party/rust/crates/chrono/` 为正确版本
3. 或修改 `project/libra/toolchains/BUCK` 使用 `prelude//toolchains:demo.bzl` 中的 `system_demo_toolchains()`（与 monorepo 根 `toolchains/BUCK` 一致）

---

## 最终验证结果（2026-02-25）

所有 FUSE/Scorpio 层面的问题修复后，在 `http://git.gitmega.com` 的 monorepo 上运行完整测试：

```
buck2 targets //project/libra/...   → 17 targets, exit=0 ✅
buck2 targets //...                 → 2772 targets, exit=0 ✅
buck2 build //project/libra:libra   → 5322/5397 actions (98.6%), 最终停于 hyper-util 0.1.18 编译错误
```

**`buck2 build` 的失败原因是 monorepo 的 Rust 依赖兼容性问题**（`hyper-util 0.1.18` API 与 `hyper 1.x` 不兼容），非 FUSE/Scorpio 问题。Targets 解析和大部分编译都在 FUSE 挂载上正常完成。

---

## 架构说明：为什么用 Buck2？Orion 的设计

### Orion 的职责

Orion 是一个 **CI/CD 构建服务**，用于代码审查（Code Review）中的增量构建验证：

```
提交 CL (Change List)
  → Orion 收到 WebSocket 构建请求
  → 挂载两个 FUSE mount（通过 Scorpio Antares API）:
      ① old_repo  — 基线版本（无 CL），作为 "before" 快照
      ② new_repo  — 基线 + CL 变更层，作为 "after" 快照
  → buck2 targets（分析 old/new 两个快照，找出受 CL 影响的 target）
  → buck2 build（编译受影响的 target）
  → 返回构建结果给代码审查系统
  → 卸载 FUSE mount
```

### 为什么用 Buck2？

| 特性 | 说明 |
|------|------|
| **增量构建分析** | Buck2 的 `buck2-change-detector` 能精确计算哪些 target 被 CL 影响，避免全量编译 |
| **确定性构建** | Buck2 沙箱化每个 action，保证相同输入产出相同输出 |
| **远程执行兼容** | Buck2 原生支持远程执行（RE），可分发编译任务到集群 |
| **Cell/Project 分层** | monorepo 中多个子项目（如 `project/libra`）通过 cell 机制隔离，同时共享 toolchain |

### 为什么需要 FUSE？

传统 CI 需要 `git clone` 完整仓库（对大型 monorepo 可能耗时数分钟到数小时）。
FUSE 挂载让 Buck2 直接在虚拟文件系统上构建，**只拉取实际访问的文件**，显著降低启动成本。

---

## 优化方案：`ready` 等待时间（从初版到当前版）

### 一、问题抽象

挂载可用分两段：

| 阶段 | 机制 | 目标 |
|------|------|------|
| **Phase 1** | Dicfuse `load_dir_depth()`（网络→内存） | 避免首批 `statx` 穿透到远端 HTTP |
| **Phase 2** | `deep_preload_walk()`（FUSE 挂载点遍历） | 把热点再推入 Linux 内核 FUSE 缓存 |

最初版本把 Phase 2 也放在 `ready` 阻塞链路中，导致 `ready` 慢到 141s 级别。

---

### 二、演进路径（版本化）

#### V0（初版，问题态）

- `Ready` 依赖 `deep_preload_walk` 完成。
- `deep_preload_walk` 单线程 DFS + 全量 `metadata()`。
- 结果：`mount -> ready` 被卡在分钟级。

#### V1（主路径解耦）

- 决策：**Phase 1 完成即 Ready**，Phase 2 改后台 best-effort。
- 判断依据：Phase 1 完成后 Dicfuse 内存缓存已热，`statx` 即使穿透到 FUSE daemon，也通常是内存命中（毫秒级），不再是网络 RTT（百毫秒级）。
- tradeoff：
  - 收益：CI 主链路从分钟级降到毫秒/秒级。
  - 代价：内核缓存不是“开场即满”，但可接受。

#### V2（并行化）

- `deep_preload_walk` 改有界并行 worker pool。
- 增加 `ANTARES_DEEP_PRELOAD_WORKERS`（`1..=64`，默认自动 `2..=8`）。
- tradeoff：
  - 收益：后台预热 wall-time 明显下降。
  - 风险：并发过高会放大 FUSE 调度开销，因此做上限和可调。

#### V3（竞态收敛）

- 新增 `preload_cancel` 协作取消。
- 在 `delete_mount/build_cl/clear_cl/shutdown` 的 unmount 前统一 `preload_cancel=true`。
- 目的：避免 remount/unmount 窗口里旧 preload 继续扫目录，产生 ENOENT 噪声与额外竞态。

#### V4（本次继续压榨）

- `deep_preload_walk` 新增策略：
  - `ANTARES_DEEP_PRELOAD_MODE`：`scan`（默认）/`hotset`/`dirs`/`full`
  - `ANTARES_DEEP_PRELOAD_MAX_DEPTH`：默认 `4`
  - `ANTARES_DEEP_PRELOAD_MAX_MS`：默认 `8000`（`0` 表示不设上限）
- 当前默认策略：`scan_only + max_depth=4`（并行遍历目录，不主动全量 `metadata`）。
- 判断依据：
  1. 当前 `libfuse-fs` 已开启 `readdirplus`，目录扫描已能带来大量 entry/attr 预热价值；
  2. 全量 `metadata` 的边际收益低于其 FUSE 往返成本；
  3. Phase 2 是后台优化，优先目标是“更快收敛 + 更低扰动”。
  4. 增加时间预算后，避免后台预热无限制占用资源。

---

### 三、实测数据（同机同仓，约 1615 entries）

| 版本/策略 | `mount -> Ready` (Phase 1) | Phase 2 耗时 | 备注 |
|----------|-----------------------------|--------------|------|
| V0（阻塞 + 单线程全量） | ~141s | ~128.9s | ready 被 Phase2 拖死 |
| V2（并行，全量/高触达） | ~100-150ms | ~17.3s（workers=8） | 主链路已解耦 |
| V2（并行调优） | ~100-150ms | ~9.5s（workers=16） | 主机调优可进一步下降 |
| V4（默认：scan + depth=4） | ~100-150ms | **~9.36s**（workers=8，`metadata_touches=0`） | 当前默认 |

结论：主链路 `ready` 已稳定在毫秒级；后台 Phase 2 已从 128s 级别压到约 9~10s。

---

### 四、`cl` 在线切换安全模式（Safe-Switch）

本次在 `build_cl/clear_cl` 增加了“控制面安全切换模式”：

1. **标记 Quiescing**：进入 `MountLifecycle::Quiescing`
2. **拒绝新控制面操作**：同 mount 的并发切换/删除请求会被拒绝（而不是继续并发推进）
3. **取消旧 preload**：`preload_cancel=true`，防止 remount 窗口旧 walker 继续扫目录
4. **短静默窗口**：`ANTARES_CL_QUIESCE_GRACE_MS`（默认 150ms，可调）后再执行 remount
5. **卸载策略优化**：先尝试 `fusermount -u`（短超时），失败再 fallback `-uz`，缩短旧句柄窗口

#### 关键 tradeoff

- 这套 Safe-Switch 能显著降低**控制面并发冲突**和旧 preload 噪声。
- 但无法在当前架构下 100% 阻断“客户端已持有 fd 的数据面 IO”：
  - `build_cl/clear_cl` 本质仍是 unmount/remount；
  - 已打开 fd 的进程在切换窗口仍可能见到瞬时 `ENOENT/ESTALE`。
- 因此在线 `cl` 切换建议仍是：
  1. 先 quiesce 客户端（停活跃 buck2/编译进程）
  2. 执行 `build_cl/clear_cl`
  3. 切换后再恢复客户端并重试一次关键命令

---

### 五、与 Orion / CI 业务匹配性（幂等 + 鲁棒）

#### 幂等性

- `create_mount(job_id/build_id)` 任务维度幂等（重复请求返回同一 mount）。
- 参数不一致（同任务不同 path/cl）会拒绝，避免误复用。
- 非稳定状态（如 `Quiescing/Unmounting`）不会返回“伪成功”。

#### 鲁棒性

- 并发重复创建会回滚孤儿挂载，避免泄漏。
- `build_cl/clear_cl` remount 失败有回滚路径，不静默成功。
- preload 统一可取消，显著降低 remount 窗口噪声。
- Ready 与 Phase2 解耦，CI 主流程不再被后台预热阻塞。

---

### 六、`cl` 接口是否还会报文档开头那类错

针对开头日志（`open_by_handle_at ESTALE` + `Failed to connect to buck daemon`）：

- 根因主链路是“冷启动 `statx` 风暴 -> buck2 daemon 超时重启循环”，不是单纯 `cl` 接口。
- 现在主链路已通过“Phase1 即 Ready + daemon isolation + 预热优化”明显降风险。
- `ESTALE` 在删除/切换窗口仍可能偶发（语义层面），但日志级别与错误映射已优化，且控制面并发风险已进一步收敛。

---

### 七、是否必须等待 `ready=true`

| 场景 | 是否建议等待 |
|------|--------------|
| Orion CI（`buck2 targets/build`） | ✅ 建议等待（等 Phase 1 即可） |
| 交互式调试 | ⚠️ 可不等，按需访问 |
| `build_cl/clear_cl` 切换后 | ✅ 建议重新等一次 `ready` |

---

## 八、2026-02-25 实机启动服务验证：`tokio::spawn` 并发行为

> 目标：验证“一个机器是否能同时跑多个构建任务”，以及“同 `build_id` 重复下发是否会并发执行”。

### 1) 验证环境

- 机器：`47.79.95.33`
- 仓库：`/root/jerry/mega`
- 启动组件：
  - `orion` worker（真实二进制）
  - 本地 mock WS/HTTP 服务（Node）
    - WS: `ws://127.0.0.1:8004/ws`
    - Scorpio API mock: `http://127.0.0.1:3725`

### 2) 启动命令

```bash
# mock server（提供 /ws + /antares/mounts）
nohup node /tmp/orion-mock/mock_server.js > /tmp/orion-mock/mock.log 2>&1 &

# 启动 orion，连接到 mock
cd /root/jerry/mega
timeout 180s env \
  SERVER_WS=ws://127.0.0.1:8004/ws \
  SCORPIO_API_BASE_URL=http://127.0.0.1:3725 \
  ORION_WORKER_ID=debug-worker-01 \
  cargo run -p orion --bin orion > /tmp/orion-mock/orion.log 2>&1
```

### 3) 场景 A：两个不同 `build_id` 几乎同时下发

mock 在 50ms 间隔下发两个任务：

- `11111111-1111-4111-8111-111111111111`
- `22222222-2222-4222-8222-222222222222`

关键观测（摘录）：

```text
[WS] send task1 ...1111...
[WS] send task2 ...2222...

[HTTP] /antares/mounts start #1 job_id=...1111-old-1 inflight=1
[HTTP] /antares/mounts start #2 job_id=...2222-old-1 inflight=2
```

结论：同一台机器上，两个构建任务会并发推进，`maxInflightMount=2`。

### 4) 场景 B：同一个 `build_id` 重复下发两次

第二轮将 task2 的 `build_id` 改为与 task1 相同（完全重复 ID）。

关键观测（摘录）：

```text
# Orion 日志里，同一个 build_id 出现两次“Received build request”
[Task 1111...] Received build request.
[Task 1111...] Received build request.

# mock 侧 mount 请求也并发进入
[HTTP] /antares/mounts start #1 job_id=...1111-old-1 inflight=1
[HTTP] /antares/mounts start #2 job_id=...1111-old-1 inflight=2
```

结论：当前 worker 侧没有对 `build_id` 做“运行中去重”，同 ID 会并发执行。

### 5) 对“会不会丢新请求”的解释

- 不是“机器不能并发”，而是“并发控制策略不完整”。
- 现状是收到任务就 `tokio::spawn`，没有全局队列/限流/同 ID 去重。
- 当后端（如 Antares 的 job_id 语义）拒绝重复或处于过渡状态时，上层看起来就像“第二个请求丢了/被覆盖”。

### 6) 本次验证结论（可复现）

1. 单机可以同时跑多个构建任务（并发成立）。
2. 同 `build_id` 也会并发跑（无运行中去重）。
3. “已有构建时再次触发报错/看似丢请求”是并发策略与后端幂等/状态机交互导致，不是 `Uuid::parse_str` 代码本身的数据竞争问题。

### 7) 验证后清理

```bash
kill $(cat /tmp/orion-mock/mock.pid 2>/dev/null) 2>/dev/null || true
kill $(cat /tmp/orion-mock/mock_dup.pid 2>/dev/null) 2>/dev/null || true
```

---

## 九、2026-02-25 补充验证（补齐 Buck 文件后）

验证结论：不是同一个报错。

- 补齐后，不再报缺少 .buckconfig。
- 新错误变为：unexpected argument --isolation-dir found。
- 上层错误变为：buck2 targets failed after 2 attempts for repo /root/jerry/mega/。
- 同 build_id 并发触发现象仍存在（两次 Received build request，mount 并发峰值为 2）。

---

## 十、2026-02-25 再补测（补齐环境后）

这次把环境缺失问题补齐后重新验证：

1) 修复了 orion 中 buck2 targets 的参数顺序问题：
   - 文件：orion/src/buck_controller.rs
   - 调整为先传 --isolation-dir，再传 targets 子命令参数。

2) 使用干净 worktree 作为挂载根，避免本地未跟踪目录干扰：
   - 挂载根：/tmp/orion-mock/mega-clean
   - 该目录包含完整 .buckconfig。

3) 重复 build_id 压测结果：
   - 仍可看到同一 build_id 被并发接收两次。
   - mock 侧 mount 并发峰值仍为 2。
   - 这次不再出现 .buckconfig missing，也不再出现 unexpected argument --isolation-dir。

4) 本轮业务输出：
   - 两条重复 build_id 的任务都走到了 RunningBuild。
   - 最终都返回 Build succeeded（exit code 0）。
   - 期间日志可见 NO BUILD TARGET PATTERNS SPECIFIED（无目标可构建），但任务状态为成功。

结论：
- 环境缺失相关错误已清理。
- “同 build_id 并发执行”问题依旧稳定可复现，根因仍是 worker 侧缺少运行中去重/排队。

## 十一、2026-02-25 Root Cause 结论（并发复现 + Buck daemon 同类异常，修复前快照）

### 1) 主根因（确定性）

Worker 侧没有“运行中去重/排队/限流”，导致同一个 `build_id` 可以被并发执行。

代码证据：

- `orion/src/ws.rs`：`process_server_message()` 收到 `TaskBuild` 后直接 `tokio::spawn`（约第 176 行），没有 in-flight 检查。
- `orion/src/api.rs`：`buck_build()` 内再次 `tokio::spawn`（约第 56 行）并立即返回 `Build task has been accepted and started.`，也没有去重。

这意味着：重复消息会被“立即扇出”为多条独立构建流水线。

### 2) 复现实证（同 build_id 并发）

日志证据：

- `/tmp/orion-repro-orion.log`
  - 同一个 `build_id=33333333-3333-4333-8333-333333333333` 出现 4 次 `Received build request`（11/15/23/31 行）。
- `/tmp/orion-repro-ws.log`
  - 同一个 `build_id` 收到 4 次 `TaskAck`（5/7/9/11 行）。

结论：不是“机器不能并发”，而是“同 ID 被并发执行”。

### 3) 与 Buck daemon 异常的因果链（同类，不一定逐字一致）

在“同 ID 并发 + 挂载生命周期抖动（unmount/remount/handle 失效）”条件下，Buck2 daemon 容易进入重连/重启/锁冲突窗口，表现为你日志里的同类异常：

- 我这次强制制造挂载抖动后，`/tmp/buck-kill-repro.err` 出现：
  - `Starting new buck2 daemon...`
  - `Error initializing DaemonStateData`
  - `disk I/O error`
  - `Error code 3850: I/O error in the advisory file locking layer`
- 历史 Scorpio 日志 `/var/log/scorpio.log` 也有同类 FUSE 句柄失效：
  - 115/118/301/304 行：`open_by_handle_at failed ... StaleNetworkFileHandle (code 116)`

说明：Buck daemon 报错是并发冲突 + 底层句柄/挂载状态异常的放大结果，不是单点 Buck 参数问题。

### 4) 为什么“像丢请求”

当同 `build_id` 被并发触发时，多个任务会竞争同一业务主键（`build_id`）的状态上报；上层只看最终状态时，会出现“第二次触发没生效/像被吃掉”的体感。

### 5) 本次 root cause 归档

- **Primary root cause**：worker 缺少 in-flight 去重与队列化（确定性逻辑缺陷）。
- **Secondary amplifier**：FUSE mount 生命周期抖动触发 handle stale，放大为 buck daemon 重连/锁层 I/O 异常（运行时症状）。


## 十二、2026-02-25 修复落地（worker 去重 + 队列）

### 1) 代码改动

- `orion/src/ws.rs`
  - 新增 in-flight 集合（`IN_FLIGHT_BUILD_IDS`）并通过 `InFlightBuildGuard` 做生命周期管理。
  - 收到 `TaskBuild` 时先做 `build_id` 去重：重复请求直接 ACK（已在运行，忽略重复）。
  - 新增全局并发槽（`BUILD_SLOTS`），由 `ORION_MAX_CONCURRENT_BUILDS` 控制（默认 `1`）。
  - 非重复任务先 ACK `accepted and queued`，再等待队列槽位执行构建。
  - 增加单测：`duplicate_build_id_is_rejected_while_guard_is_alive`。

- `orion/src/api.rs`
  - 去掉内部二次 `tokio::spawn`，改为在当前 async 流程中直接执行构建。
  - 构建结束后发送 `TaskBuildComplete`，并返回最终 `BuildResult`。

### 2) 行为变化

1. 同一个 `build_id` 的重复下发不再并发执行，避免同主键状态竞争。
2. 非重复任务进入有界队列，避免无上限 `tokio::spawn` 扇出。
3. 默认并发上限为 `1`，可通过环境变量调大：

```bash
export ORION_MAX_CONCURRENT_BUILDS=2
```

### 3) 预期收益

- 降低 “已有构建时再次触发报错/像丢请求” 的概率。
- 降低挂载抖动引发 buck daemon 重启/锁冲突的放大效应。


## 十三、2026-02-25 修复后复测（实机）

### 1) 复测命令

```bash
# 场景 A：同 build_id 重复下发
nohup node /tmp/orion-mock/mock_server_dup.js > /tmp/orion-mock/repro_fix_mock.log 2>&1 &
timeout 70s env \
  SERVER_WS=ws://127.0.0.1:8004/ws \
  SCORPIO_API_BASE_URL=http://127.0.0.1:3725 \
  ORION_WORKER_ID=fix-worker-1 \
  ORION_MAX_CONCURRENT_BUILDS=1 \
  cargo run -p orion --bin orion > /tmp/orion-mock/repro_fix_orion.log 2>&1

# 场景 B：不同 build_id 几乎同时下发
nohup node /tmp/orion-mock/mock_server.js > /tmp/orion-mock/repro_fix_mock2.log 2>&1 &
timeout 70s env \
  SERVER_WS=ws://127.0.0.1:8004/ws \
  SCORPIO_API_BASE_URL=http://127.0.0.1:3725 \
  ORION_WORKER_ID=fix-worker-2 \
  ORION_MAX_CONCURRENT_BUILDS=1 \
  cargo run -p orion --bin orion > /tmp/orion-mock/repro_fix_orion2.log 2>&1
```

### 2) 场景 A 结果（同 build_id）

关键证据：

- `/tmp/orion-mock/repro_fix_orion.log`
  - 只有 1 次 `Received build request`。
  - 后续重复请求被日志标记为 `Duplicate build request ignored`。
- `/tmp/orion-mock/repro_fix_mock.log`
  - 收到两条 `TaskAck`：
    - 第一条：`accepted and queued`
    - 第二条：`already in progress; duplicate request ignored`
  - `SUMMARY` 显示 `maxInflightMount=1`。

结论：同 `build_id` 已不再并发执行。

### 3) 场景 B 结果（不同 build_id）

关键证据：

- `/tmp/orion-mock/repro_fix_orion2.log`
  - 两个不同 `build_id` 都被接受，但第二个要等第一个构建阶段完成后才开始 `Starting build after queue wait`。
- `/tmp/orion-mock/repro_fix_mock2.log`
  - `SUMMARY` 显示 `maxInflightMount=1`。

结论：队列生效，默认并发上限 `1` 下不会出现两条构建流水线同时推进。

### 4) 备注

- 本轮复测验证了“去重 + 队列”确实生效。
- Buck 本身是否构建成功仍取决于挂载内容（例如 `.buckconfig` 是否存在），但并发放大问题已被压制。


## 十四、2026-02-25 服务端落地（方案 1 + 2）

### 1) 方案 1：唯一任务主键 / 幂等提交

已落地内容：

- `api-model/src/buck2/api.rs`
  - `TaskBuildRequest` 新增可选字段 `build_id`（幂等键）。
- `orion-server/src/api.rs`
  - `/task` 在创建新任务前先检查 `build_id`：
    - 若该 `build_id` 已存在（queued/leased/building/completed/failed/interrupted），直接返回已有状态，不重复创建新构建。
  - 无 `build_id` 时保持原行为（服务端生成）。
- `orion-server/src/scheduler.rs`
  - `enqueue_task_with_build_id` 增加 in-flight/queue 去重检查。
  - 入队前先持久化 build 记录（`builds` 表），保证 ACK/可观测状态基于已落库数据。

结果：重复请求不再创建重复构建，返回现有构建状态（幂等）。

### 2) 方案 2：可恢复队列（Lease）

已落地内容：

- `orion-server/src/scheduler.rs`
  - 新增 `leased_builds`（已派发未确认）与 `lease_timeout`。
  - 新增环境变量：`ORION_LEASE_TIMEOUT_SECS`（默认 30 秒）。
  - 派发任务后创建 lease；worker `TaskAck(success=true)` 后清理 lease。
  - `TaskAck(success=false)` 或 lease 超时：任务回到队列头部并重新调度。
  - queue manager 增加 lease reclaim 后台循环（3 秒检查一次）。
- `orion-server/src/api.rs`
  - 增加 `TaskAck` 处理（驱动 lease 状态机）。
  - 收到 `TaskBuildOutput/TaskPhaseUpdate/TaskBuildComplete` 时清理 lease。
  - retry 路径重新下发时也创建 lease，避免“重试任务无 lease 保护”。

结果：出现“派发成功但 worker 未确认/确认丢失”时，任务可自动回收并重排队，不会长期悬挂。

### 3) 本轮验证

执行命令：

```bash
cargo +nightly fmt --all
cargo check --manifest-path orion-server/Cargo.toml --all-targets
cargo clippy --manifest-path orion-server/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path orion-server/Cargo.toml scheduler::tests::test_task_queue_fifo -- --nocapture
cargo test --manifest-path orion-server/Cargo.toml scheduler::tests::test_queue_capacity -- --nocapture
cargo test --manifest-path orion-server/Cargo.toml scheduler::tests:: -- --nocapture
```

结果：全部通过。

### 4) 性能说明（针对你的担心）

- 幂等检查是 O(1) 内存检查 + 单次主键查询，开销很小。
- lease 回收是周期扫描 in-memory map，默认粒度 3 秒，通常远小于构建时长。
- 默认不会降低单任务构建速度，主要增加的是“异常场景可恢复性”和“重复请求去重”。

## 十五、2026-02-26 改进后回归复测（本轮）

> 目标：确认在“worker 去重+队列”与“server 幂等+lease”落地后，之前的核心问题是否仍存在。

### 1) 场景 A：同 `build_id` 重复下发（real buck mountpoint）

执行：

```bash
nohup node /tmp/orion-mock/mock_server_realbuck.js > /tmp/orion-mock/retest_realbuck_mock.log 2>&1 &
timeout 120s env   SERVER_WS=ws://127.0.0.1:8004/ws   SCORPIO_API_BASE_URL=http://127.0.0.1:3725   ORION_WORKER_ID=retest-worker-realbuck   ORION_MAX_CONCURRENT_BUILDS=1   cargo run -p orion --bin orion > /tmp/orion-mock/retest_realbuck_orion.log 2>&1
```

结果要点：

- 第二次同 ID 请求被忽略：
  - `Duplicate build request ignored because this build is already in-flight.`
- mock 统计：
  - `[SUMMARY] totalMount=2 maxInflightMount=1`
- 未出现本次关注的 buck daemon 同类异常关键字：
  - `Failed to connect to buck daemon`
  - `Starting new buck2 daemon...`
  - `Stale file handle`

结论：同 `build_id` 并发执行问题本轮未复现。

### 2) 场景 B：不同 `build_id` 几乎同时下发（队列行为）

执行：

```bash
nohup node /tmp/orion-mock/mock_server.js > /tmp/orion-mock/retest_fix_mock2.log 2>&1 &
timeout 120s env   SERVER_WS=ws://127.0.0.1:8004/ws   SCORPIO_API_BASE_URL=http://127.0.0.1:3725   ORION_WORKER_ID=retest-worker-queue   ORION_MAX_CONCURRENT_BUILDS=1   cargo run -p orion --bin orion > /tmp/orion-mock/retest_fix_orion2.log 2>&1
```

结果要点：

- 两个任务都会被 ACK，但串行进入构建：
  - 先执行 `1111...`，完成后才出现 `2222... Starting build after queue wait`
- mock 统计：
  - `[SUMMARY] totalMount=8 maxInflightMount=1`
- 失败原因为 mock 挂载内容不含 `.buckconfig`，属于测试数据问题，不是并发放大问题：
  - `Expected to find a .buckconfig file.`

结论：并发上限与排队生效，未出现“两个构建流水线同时推进”的旧问题。

### 3) 服务端调度单测回归

执行：

```bash
cargo test --manifest-path orion-server/Cargo.toml scheduler::tests:: -- --nocapture
```

结果：4/4 通过（lib 与 bin 各一轮）。

### 4) 本轮总论

- 旧核心问题（同 ID 并发导致状态竞争、并发放大引发 buck daemon 异常）本轮未复现。
- 当前可见失败主要是“测试输入缺 `.buckconfig`”这类业务数据/环境问题。
- 若线上仍有偶发 buck daemon 异常，下一步应重点排查真实挂载稳定性（FUSE handle 生命周期、底层 remount 抖动）。


## 十六、2026-02-26 CL 报错复核 + 压测（本轮）

> 你给的两条 CL 链接（`WQJ6U3ON` / `CG1RJJAW`）页面本身需要登录态，CLI 侧无法直接拿到 diff 内容；本轮按你贴的错误日志做了等价复现与压测。

### 1) 目标错误复核

你提供的核心报错关键词：

- `Could not connect to buck2 daemon (buck2 daemon is not running), starting a new one...`
- `Error initializing DaemonStateData`
- `disk I/O error / advisory file locking layer`（你的线上日志）

本轮在压测中稳定复现到同类链路（daemon 初始化失败 + 重连失败），本机复现实例的底层原因主要是：

- `OS file watch limit reached`

说明：底层具体 errno 在不同机器/文件系统上可能不同（你是 lock-layer I/O，我这里是 inotify/watcher 上限），但都落在 **“buck daemon 频繁拉起失败 -> 连接失败重试 -> 构建阶段失败”** 这一类问题。

### 2) 压测结果（修复前快照）

#### A. 固定 mountpoint（非真实场景，对照组）

- `ORION_MAX_CONCURRENT_BUILDS=1`：20 任务，`total_ms=18821`，`maxInflightMount=1`，无 daemon 异常。
- `ORION_MAX_CONCURRENT_BUILDS=2`：20 任务，`total_ms=10053`，`maxInflightMount=2`，无 daemon 异常。
- `ORION_MAX_CONCURRENT_BUILDS=4`：20 任务，`total_ms=5756`，`maxInflightMount=4`，无 daemon 异常。
- `ORION_MAX_CONCURRENT_BUILDS=8`：40 任务，`total_ms=7749`，`maxInflightMount=8`，无 daemon 异常。

#### B. unique mountpoint（更接近真实场景）

- `ORION_MAX_CONCURRENT_BUILDS=4`：30 任务，`total_ms=22979`，`maxInflightMount=4`，并大量出现：
  - `Could not connect to buck2 daemon...`
  - `Error initializing DaemonStateData`
  - `OS file watch limit reached`
- 在该状态继续跑 `ORION_MAX_CONCURRENT_BUILDS=1` 也会被“资源已耗尽”连带影响，错误持续。

### 3) 本轮落地修复（代码）

- 文件：`orion/src/buck_controller.rs`
- 变更：新增 `cleanup_buck2_daemon(repo_path)`，在以下路径做 best-effort 清理：
  - `get_repo_targets()` 每次 `buck2 targets` 完成后
  - `build()` 中 `buck2 build` 完成后
- 目的：避免 unique mount + unique isolation 场景下，buck daemon 长时间堆积导致资源耗尽。

### 4) 修复后复测

- unique mount + `ORION_MAX_CONCURRENT_BUILDS=1`（12 任务）
  - `success_true=12`, `success_false=0`
  - `total_ms=49749`, `maxInflightMount=1`
  - 未出现 daemon 初始化失败关键字

- unique mount + `ORION_MAX_CONCURRENT_BUILDS=4`（20 任务）
  - `success_true=0`, `success_false=20`
  - 仍出现大量 `OS file watch limit reached` + `Error initializing DaemonStateData`

### 5) 结论（针对“性能如何”）

- **稳定性优先配置**：在当前这台机器上，建议 `ORION_MAX_CONCURRENT_BUILDS=1`。
- 并发调高（尤其真实 unique mount）会快速触发 buck daemon 初始化失败，吞吐虽看似高但失败率不可接受。
- 若必须提升并发，需要同时做主机层容量扩容（如 inotify 限制）与 Buck/worker 并发策略调整。

### 6) 机器现状（本机）

```bash
fs.inotify.max_user_instances = 128
fs.inotify.max_user_watches = 120417
fs.inotify.max_queued_events = 16384
```

## 十七、2026-02-26 CL 相关接口测试（本轮）

### 1) Scorpio（Antares）CL 接口单测

执行（节选）：

```bash
cargo test --manifest-path scorpio/Cargo.toml test_build_cl_success -- --nocapture
cargo test --manifest-path scorpio/Cargo.toml test_http_build_cl -- --nocapture
cargo test --manifest-path scorpio/Cargo.toml test_http_clear_cl -- --nocapture
cargo test --manifest-path scorpio/Cargo.toml test_create_mount_with_cl -- --nocapture
cargo test --manifest-path scorpio/Cargo.toml test_same_path_different_cl_allowed -- --nocapture
cargo test --manifest-path scorpio/Cargo.toml test_duplicate_path_cl_rejected -- --nocapture
```

结果：13 个 CL 相关用例全部通过（build/clear/http/重复挂载策略）。

### 2) Mono CL Router 单测

执行：

```bash
cargo test --manifest-path mono/Cargo.toml api::router::cl_router::test:: -- --nocapture
```

结果：5/5 通过。

### 3) Mono CL Merge 集成测试

执行：

```bash
cargo test --manifest-path mono/Cargo.toml --test cl_merge_integration test_cl_merge_integration -- --ignored --nocapture
```

结果：失败，环境缺失：

- `failed to execute guestfish`
- `No such file or directory (os error 2)`

### 4) Orion Antares API 集成测试（含 CL 场景）

执行：

```bash
cargo test --manifest-path orion/Cargo.toml --test antares_api_integration_test -- --nocapture
```

结果：8 个用例中 5 通过、3 失败（均为带 CL 的创建/复用场景）。
失败根因来自运行中服务返回：

```text
{"error":"failed to fetch CL files: HTTP 500 Internal Server Error","code":"INTERNAL_ERROR"}
```

### 5) 在线接口冒烟（直接 curl）

- `POST /antares/mounts`（无 CL）返回 200，可创建并卸载。
- `POST /antares/mounts/{mount_id}/cl`（带 CL）返回 500，错误同上（CL 文件源接口 500）。
- `DELETE /antares/mounts/{mount_id}/cl` 在无 CL 层时返回 400（语义正确）。

### 6) 兼容性小修

为了让 mono 相关测试可编译，补了一个字段：

- `ceres/src/build_trigger/dispatcher.rs`
  - `TaskBuildRequest` 新增 `build_id: None`


## 十八、2026-02-26 服务启动后 CL 接口功能/并发联调（本轮）

> 服务状态：`scorpio` 进程在线，监听 `0.0.0.0:2725`（cwd: `/root/orion-runner`）。

### 1) 功能接口回归（按功能链路）

基于 `http://localhost:2725/antares` 做了完整链路：

- `GET /health` -> 200
- `POST /mounts`（无 CL）-> 200
- `GET /mounts/{id}` -> 200
- `GET /mounts/{id}/ready` -> 200
- `GET /mounts` -> 200
- `POST /mounts/{id}/cl`（`CG1RJJAW`）-> 200
- `DELETE /mounts/{id}/cl` -> 200
- `POST /mounts/{id}/cl`（无效 `ILDAJHOI`）-> 500（上游 CL 文件接口返回 internal error）
- `DELETE /mounts/{id}` -> 200

补充：用户给的两个 CL 均可成功 build CL：

- `WQJ6U3ON`: `build_cl` 200（约 3066ms）
- `CG1RJJAW`: `build_cl` 200（约 544ms）

### 2) 并发接口验证

#### A. 同 `job_id` 并发 `POST /mounts`

- 8 并发请求结果：`1 x 200` + `7 x 400`
- 400 错误内容：`job_id/build_id 'xxx' is already mounted`
- 结论：
  - 最终只创建 1 个挂载（`GET /mounts/by-job/{job_id}` 可查到）
  - **但并发重复请求不会统一返回 200 幂等结果**，存在“调用方看到失败”的体感问题

#### B. 无 `job_id` 的 legacy 并发重复挂载（同 path+cl）

- 5 并发：`1 x 200` + `4 x 400`
- 错误：`path /project with cl None is already mounted`
- 结论：符合“拒绝重复 (path, cl)”策略

#### C. 不同 `job_id` 并发（同 path+cl）

- 4 并发：`4 x 200`
- 创建了 4 个不同 mount_id
- 结论：符合“不同 job 独立挂载”策略

#### D. 同一 mount 并发 `build_cl`

- 4 并发：`1 x 200` + `3 x 400`
- 失败请求在切换窗口被拒绝（状态机保护）
- 结论：符合 quiesce/switch 保护策略

#### E. 同一 mount 并发 `DELETE /mounts/{id}`

- 4 并发：`1 x 200` + `3 x 400`
- 失败错误：`mount ... is currently in state Unmounting; retry after switch/unmount completes`
- 结论：这是预期的“状态保护式拒绝”，非数据丢失

### 3) 本轮结论

- 功能链路总体可用，`WQJ6U3ON / CG1RJJAW` 都可成功 build CL。
- 主要并发风险点：
  - **同 `job_id` 并发创建不会幂等返回 200（多数请求返回 400）**。
  - 资源与状态层面虽未丢失挂载，但上游调用会感知到失败，建议服务端改为“返回已有 mount（200）”的强幂等语义。

### 4) 产物

- 并发测试汇总：`/tmp/antares_api_concurrency_test.json`
- 并发脚本：`/tmp/antares_api_concurrency_test.py`


## 十九、2026-03-08 新问题：`get_build_targets` 阶段出现 ENOTCONN

### 1) 现象

最新现场报错：

```text
Error getting build targets: Fail to get cells: Transport endpoint is not connected (os error 107)
```

这次错误不再是「buck2 daemon 启动超时」主导，而是更早发生在 `get_build_targets()` 的 `buck2 cells` 阶段：

- 代码位置：`orion/src/buck_controller.rs`
- 调用链：`build()` -> `get_build_targets()` -> `Buck2::cells()`
- 失败点说明：buck2 还没真正开始 build，只是在读取 cells / config 时访问 FUSE 挂载点，就遇到了 `ENOTCONN`

### 2) 当前推断的直接原因

#### A. scorpiofs 直连挂载返回后，挂载点可能还未完全可访问

当前 Orion 已切到 `scorpiofs::AntaresManager::mount_job()` 直连模式。`mount_job()` 返回时，FUSE session 已经 spawn，但从 scorpiofs 自身测试也能看出：**mount() 返回并不等价于挂载点上的目录立即可稳定访问**。

如果 Orion 在挂载刚返回时立刻执行：

- `preheat_shallow()`
- `buck2 cells`
- `buck2 audit config`

就可能撞上内核侧还未完全 ready / FUSE session 尚未稳定的窗口，最终表现为：

```text
Transport endpoint is not connected (os error 107)
```

#### B. 当前分支仍保留了“调试期不主动 unmount”的逻辑，容易累积脏挂载

在本轮修复前，`MountGuard::drop` 被改成了“只打日志、不卸载”，并且 build 结束后也显式跳过了：

- `mount_guard.unmount()`
- `mount_guard_old_repo.unmount()`

这会导致：

- 挂载点持续堆积；
- 旧 FUSE session /旧 buck2 daemon 无法及时回收；
- stale handle / ENOTCONN / daemon 连接类错误更容易放大。

这和当前现场的新报错高度相关。

#### C. build 早返回路径没有统一回收 target build tracker / buck2 daemon

旧逻辑在 `child.wait()` 已经拿到 `exit_status` 时会提前 return，导致：

- `cleanup_buck2_daemon()` 不一定执行完整；
- `target_build_track` cancellation / join 也可能漏掉。

这类资源泄漏不会直接制造 ENOTCONN，但会让 worker 长时间运行后越来越不稳定。

### 3) 本轮修复策略

本轮代码修改采用三条线同时收敛：

1. **挂载可访问性等待**
   - 新增 `wait_for_repo_mount_ready()`
   - 在 `get_build_targets()` 前，先对 old/new 两个 project root 做目录访问探测
   - 针对以下错误做短时重试：
     - `ENOTCONN` / os error 107
     - `ESTALE` / os error 116
     - `EIO`
     - `ENOENT`（刚挂载完成时子目录尚未稳定可见）

2. **仅对 FUSE/daemon 类瞬时错误继续 fresh mount retry**
   - 新增 `is_retryable_target_discovery_error()`
   - `build()` 中 target discovery 失败时，只有命中典型瞬时错误才继续 fresh mount retry
   - 同时把 target discovery 最大尝试次数从 2 提升到 3

3. **恢复正常资源回收**
   - 恢复 `MountGuard::drop` 的自动卸载
   - build 结束后恢复显式 `unmount()`
   - 统一收敛 `cleanup_buck2_daemon()` 与 target build tracker 回收路径

### 4) 新增验证

#### 单元测试

在 `orion/src/buck_controller.rs` 新增：

- `test_retryable_target_discovery_error_detects_fuse_disconnects`
- `test_wait_for_repo_mount_ready_retries_until_dir_exists`
- `test_wait_for_repo_mount_ready_times_out_for_missing_dir`

#### 手工环境回归测试（ignored test）

新增一个需要真实环境的 ignored test：

- `test_real_mount_waits_until_buck2_cells_succeeds`

用途：

- 在具备 `/dev/fuse`、`buck2`、`SCORPIO_CONFIG`、网络访问能力的机器上
- 真实挂载一次 Antares 根目录
- 等待挂载 ready
- 直接执行 `buck2 cells`
- 验证 target discovery 前置阶段不再出现 ENOTCONN

### 5) 后续建议

这个修复能先把当前最直接的 `ENOTCONN at get_build_targets` 问题压下去，但如果后续仍偶发，可以继续往下收：

- 把 mount readiness 从“文件系统探测”升级为“明确的 ready 信号/health probe”；
- 给 Antares mount / buck2 cells / buck2 build 增加分层指标（mount latency / mount probe retries / cells retry count）；
- 对 worker 重连后 lease reclaim + in-flight build 去重补一轮端到端回归。


## 二十、2026-03-08 新问题：Buck2 daemon advisory lock I/O error

### 1) 现象

在真实 FUSE 挂载环境里，`buck2` 不再只报 `ENOTCONN`，而是进一步出现：

```text
Error initializing DaemonStateData

disk I/O error
Error code 3850: I/O error in the advisory file locking layer
```

这个报错发生在 target discovery 早期阶段，不一定要等到真正执行 build：

- `buck2 audit cell --json --reuse-current-config`
- `buck2 audit config --reuse-current-config`
- `buck2 targets ...`
- `buck2 build ...`

都可能在初始化 daemon/materializer state 时被同一类锁错误卡住。

### 2) 排查结论

#### A. 之前 ignored test 的“跑满 60 秒像挂死”主要是测试自身把 tokio runtime 卡住了

旧版 `test_real_mount_waits_until_buck2_cells_succeeds` 使用：

- 默认 `#[tokio::test]`
- `std::process::Command::output()`

这会在单线程 runtime 上同步阻塞，导致 FUSE session 拿不到调度时间，表现出来就像“挂载后某些访问直接卡住”。

这一层已经先修掉：

- 测试改为 `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`
- Buck2 命令改为 `tokio::process::Command`

因此，之前那次 60 秒超时不能直接当作 host/FUSE 真正 deadlock 的证据。

#### B. 真正触发 `Error code 3850` 的根因，是 Buck2 可写状态落在 FUSE 挂载 repo 的 `buck-out`

失败现场里，可以直接在 FUSE upper 层看到 Buck2 的 SQLite 状态文件，例如：

- `/tmp/megadir/antares/upper/3859a9d0-420c-41b4-acfe-0ad3f3c5d0e9/buck-out/codex-mount-ready-test/cache/materializer_state/db.sqlite`

结合失败日志里的：

```text
Error initializing DaemonStateData
Error code 3850: I/O error in the advisory file locking layer
```

可以基本确认：

- Buck2 daemon/materializer/incremental state 的锁文件和 SQLite 数据落在 mounted repo 的 `buck-out`
- 这个 `buck-out` 实际位于 scorpiofs/FUSE overlay upper layer
- advisory lock 在这层文件系统上不稳定，最终变成 SQLite 的 I/O / locking error

换句话说，这不是单纯的 `audit cell` 命令问题，而是 **所有会初始化同一个 daemon state 的 Buck2 子命令** 都可能撞到同一把锁。

### 3) 修复策略

修复思路不是禁用 target discovery，而是把 Buck2 的可写状态从 FUSE 挂载层挪走。

本轮在 `orion/src/buck_controller.rs` 增加了本地 `buck-out` 方案：

- 新增固定根目录：`/tmp/orion-buck-out`
- 针对每个 mounted repo，根据 repo path 计算稳定 hash
- 在真正执行 Buck2 前，为 mounted repo 创建：
  - `repo/buck-out -> /tmp/orion-buck-out/buck-out-<hash>`
- target discovery 失败重试、build 结束、mount 回收时都清理对应 symlink 和本地目录

这样做之后：

- 源码读取仍然走 FUSE 挂载
- Buck2 daemon/materializer/incremental state 落到宿主机普通文件系统
- advisory lock 不再依赖 FUSE upper layer

### 4) 分阶段验证结果

ignored 环境测试也同步扩展成分阶段验证，按顺序执行：

1. `buck2 audit cell --json --reuse-current-config`
2. `buck2 audit config --reuse-current-config`
3. `buck2 targets prelude//platforms:default --json-lines`
4. `buck2 build prelude//platforms:default`

并且所有命令都显式使用同一个 isolation dir：`codex-mount-ready-test`。

成功日志显示，修复后 mounted repo 会先准备本地 `buck-out`，例如：

- `/tmp/orion-buck-out/buck-out-ed9aee1b91b59c9d`
- `/tmp/orion-buck-out/buck-out-d5c924083a512886`

随后真实环境测试通过：

- `cargo test -p orion --lib -- --nocapture`
  - 结果：`38 passed; 1 ignored`
- `cargo test -p orion test_real_mount_waits_until_buck2_cells_succeeds -- --ignored --nocapture`
  - 结果：通过
  - 最新成功样例耗时约 `3.62s`

这说明：

- `audit cell`
- `audit config`
- `targets`
- `build`

都没有再被同一个 daemon lock / advisory lock I/O error 卡住。

### 5) 与前一轮 ENOTCONN 问题的关系

前一轮文档里的 `Transport endpoint is not connected (os error 107)` 仍然是需要防守的一类 FUSE 瞬时错误，所以 `wait_for_repo_mount_ready()` 和 target discovery retry 仍然保留。

但从这次实测看，修复本地 `buck-out` 之后：

- 没有再复现 `Error code 3850`
- 也没有在 `audit cell / audit config / targets / build` 阶段复现新的 `ENOTCONN`

因此，当前更明确的结论是：

- `ENOTCONN` 负责的是“挂载刚 ready 时的可访问性窗口”
- `Error code 3850` 负责的是“Buck2 daemon state 落在 FUSE `buck-out` 上导致锁层 I/O 异常”

两者属于同一链路里的不同层次问题，本轮已经分别做了处理。

### 6) 剩余风险与后续建议

当前修复已经把最稳定可复现的 Buck2 daemon 锁问题绕开，但如果线上还出现“某些访问直接卡住”的现象，后续要继续沿宿主机/FUSE/scorpiofs 这层排查：

- 给 mount readiness 增加更明确的 health probe，而不只是目录探测
- 观测挂载后首次 `lookup/open/read` 的耗时与失败率
- 区分 `source tree 读取问题` 与 `buck-out 写状态问题`
- 若后续仍出现 daemon 级异常，可继续评估更强的隔离目录策略，或在极端场景下进一步限制 daemon 复用
