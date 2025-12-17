# Scorpio 挂载流程详解：从拉取文件到挂载

## 目录结构说明

在挂载过程中，会创建以下目录结构：

```
/tmp/antares_build_<uuid>/
├── mnt/          ← 挂载点（用户看到的工作目录）
├── upper/        ← 可写层（用户修改的文件）
├── cl/           ← CL 层（可选，变更列表相关）
└── store/        ← Dicfuse 缓存（目录树和文件内容）
```

## 各目录的作用

### 1. **store/** - Dicfuse 缓存目录

**作用**：存储 Dicfuse 从远端拉取的目录树和文件内容缓存

**存储内容**：
- **目录树信息**：通过 `fetch_dir()` 从远端 API (`/api/v1/tree/content-hash`) 获取的目录结构
- **文件内容**：通过 `fetch_file()` 从远端 API (`/api/v1/file/blob/{oid}`) 获取的文件内容
- **元数据**：inode 映射、路径映射等

**特点**：
- 只读缓存，不会修改远端内容
- 按需加载：首次访问时才从远端拉取
- 持久化：可以保存到数据库，下次启动时直接加载

**示例内容**：
```
store/
├── db/                    # SQLite 数据库（如果启用持久化）
│   └── dicfuse.db
└── (内存中的数据结构)
    ├── inodes: {1: root_item, 2: file_item, ...}
    ├── dirs: {"/": DirItem, "/project": DirItem, ...}
    └── file_contents: {inode: Vec<u8>, ...}
```

### 2. **upper/** - 可写层（Upper Layer）

**作用**：存储用户在挂载点上的所有修改

**存储内容**：
- **用户创建的新文件**
- **用户修改的文件**（通过 Copy-up 从 lower 层复制过来）
- **用户删除的文件**（通过 whiteout 标记）

**特点**：
- 可读写
- 优先级最高：读请求优先查找 upper 层
- 写操作必须落在 upper 层

**示例内容**：
```
upper/
├── third-party/
│   └── buck-hello/
│       ├── main.rs          ← 用户修改的文件（copy-up）
│       └── new_file.txt     ← 用户创建的新文件
└── project/
    └── .buck2/              ← Buck2 创建的状态文件（如果有）
```

### 3. **cl/** - CL 层（Change List Layer，可选）

**作用**：存储变更列表相关的文件（如果指定了 CL）

**存储内容**：
- CL 相关的文件修改
- 作为 lower layer 的一部分，优先级高于 Dicfuse

**特点**：
- 可选：只有在创建挂载时指定了 `cl` 参数才会创建
- 可读写：类似 upper，但作为 lower layer 的一部分

**示例内容**：
```
cl/
└── third-party/
    └── buck-hello/
        └── modified.rs      ← CL 相关的修改
```

### 4. **mnt/** - 挂载点（Mount Point）

**作用**：用户看到和操作的统一文件系统视图

**存储内容**：
- **不直接存储文件**：这是一个 FUSE 挂载点，不是普通目录
- **虚拟视图**：通过 OverlayFs 将 upper/CL/Dicfuse 合并后的视图

**特点**：
- 用户只能看到这个目录
- 所有文件操作（读/写/创建/删除）都通过这个挂载点
- 底层通过 OverlayFs 路由到相应的层

**用户看到的视图**：
```
mnt/
├── doc/
├── project/
├── third-party/
│   └── buck-hello/
│       ├── BUCK             ← 来自 Dicfuse（只读层）
│       ├── main.rs          ← 来自 upper（用户修改过）
│       └── new_file.txt     ← 来自 upper（用户创建）
└── model/
```

---

## 完整流程：从拉取文件到挂载

### 阶段 1：初始化 Dicfuse 和 Store

```rust
// 1. 创建 store 目录
let store_path = base.join("store");  // /tmp/antares_build_xxx/store
std::fs::create_dir_all(&store_path)?;

// 2. 创建 Dicfuse 实例（使用 store 作为缓存路径）
let dic = Dicfuse::new_with_store_path(store_path.to_str().unwrap()).await;
```

**此时 store/ 目录**：
- 目录已创建，但内容为空
- Dicfuse 会初始化内存中的数据结构（inodes、dirs、file_contents）

### 阶段 2：预加载目录树（import_arc）

```rust
// 3. 预加载目录树
scorpio::dicfuse::store::import_arc(dic.store.clone()).await;
```

**流程详解**：

#### 2.1 检查是否有持久化数据库

```rust
if store.load_db().await.is_ok() {
    // 如果数据库存在，直接加载，跳过网络请求
    return;
}
```

#### 2.2 初始化根目录

```rust
// 创建根目录 inode (inode = 1)
store.persistent_path_store.insert_item(
    1,
    UNKNOW_INODE,
    ItemExt {
        item: Item {
            name: "",
            path: "/",
            content_type: "directory",
        },
        hash: String::new(),
    },
);
```

#### 2.3 并发加载目录树（load_dir_depth）

```rust
// 从根目录 "/" 开始，加载到指定深度（max_depth = load_dir_depth + 2）
load_dir_depth(store.clone(), "/".to_string(), max_depth).await;
```

**加载流程**：

1. **初始请求**：
   ```
   GET /api/v1/tree/content-hash?path=/
   ↓
   返回：[
     {item: {name: "doc", path: "/doc", content_type: "directory"}, hash: "abc123"},
     {item: {name: "project", path: "/project", content_type: "directory"}, hash: "def456"},
     {item: {name: "README.md", path: "/README.md", content_type: "file"}, hash: "ghi789"},
     ...
   ]
   ```

2. **处理目录项**：
   - **目录**：加入队列，继续递归加载
   - **文件**：立即拉取文件内容

3. **拉取文件内容**（对于文件项）：
   ```
   GET /api/v1/file/blob/{oid}
   ↓
   返回：文件二进制内容
   ↓
   存储到 store.file_contents[inode] = Vec<u8>
   ```

4. **并发处理**：
   - 10 个工作线程并发处理队列中的目录
   - 每个线程处理一个目录，获取其子项，继续递归

**此时 store/ 目录**：
- 内存中已加载目录树结构（inodes、dirs）
- 已拉取的文件内容缓存在内存中（file_contents）
- 如果启用持久化，会保存到 `store/db/dicfuse.db`

**示例 store 内存结构**：
```rust
store.inodes = {
    1: DicItem {path: "/", inode: 1, children: {2, 3, 4}},
    2: DicItem {path: "/doc", inode: 2, children: {...}},
    3: DicItem {path: "/project", inode: 3, children: {...}},
    4: DicItem {path: "/README.md", inode: 4, ...},
}

store.dirs = {
    "/": DirItem {hash: "", file_list: {"/doc", "/project", "/README.md"}},
    "/doc": DirItem {hash: "abc123", file_list: {...}},
    ...
}

store.file_contents = {
    4: Vec<u8> {...},  // README.md 的内容
    ...
}
```

### 阶段 3：创建目录结构

```rust
// 4. 创建 upper、cl、mnt 目录
let mount = base.join("mnt");      // /tmp/antares_build_xxx/mnt
let upper = base.join("upper");    // /tmp/antares_build_xxx/upper
let cl = base.join("cl");          // /tmp/antares_build_xxx/cl

// AntaresFuse::new() 内部会创建这些目录
let mut fuse = AntaresFuse::new(mount, Arc::new(dic), upper, Some(cl)).await?;
```

**此时目录结构**：
```
/tmp/antares_build_xxx/
├── mnt/          ← 空目录（等待挂载）
├── upper/        ← 空目录（等待用户修改）
├── cl/           ← 空目录（可选）
└── store/        ← 已加载目录树和文件内容（内存中）
```

### 阶段 4：组装 OverlayFs

```rust
// 5. 在 mount() 内部调用 build_overlay()
fuse.mount().await?;
```

**build_overlay() 流程**：

```rust
pub async fn build_overlay(&self) -> std::io::Result<OverlayFs> {
    let mut lower_layers: Vec<Arc<dyn Layer>> = Vec::new();
    
    // 1. 如果有 CL，先加入 lower layers
    if let Some(cl_dir) = &self.cl_dir {
        let cl_layer = new_passthroughfs_layer(PassthroughArgs {
            root_dir: cl_dir,  // /tmp/antares_build_xxx/cl
            mapping: None,
        }).await?;
        lower_layers.push(Arc::new(cl_layer));
    }
    
    // 2. Dicfuse 作为最后一层 lower
    lower_layers.push(self.dic.clone());  // Dicfuse 实例（使用 store 中的缓存）
    
    // 3. Upper 层作为可写层
    let upper_layer = Arc::new(
        new_passthroughfs_layer(PassthroughArgs {
            root_dir: &self.upper_dir,  // /tmp/antares_build_xxx/upper
            mapping: None,
        }).await?
    );
    
    // 4. 组装 OverlayFs
    OverlayFs::new(Some(upper_layer), lower_layers, cfg, 1)
}
```

**层级结构**：
```
OverlayFs
├── Upper Layer (可写)
│   └── /tmp/antares_build_xxx/upper/
└── Lower Layers (只读，从上到下查找)
    ├── CL Layer (可选)
    │   └── /tmp/antares_build_xxx/cl/
    └── Dicfuse Layer
        └── store/ (内存中的目录树和文件内容)
```

### 阶段 5：挂载到内核

```rust
// 6. 挂载 OverlayFs 到挂载点
mount_filesystem(logfs, mountpoint.as_os_str()).await;
```

**挂载流程**：

1. **调用 FUSE 库挂载**：
   ```rust
   mount_filesystem(LoggingFileSystem(OverlayFs), "/tmp/antares_build_xxx/mnt")
   ```

2. **启动 FUSE session**：
   ```rust
   let fuse_task = tokio::spawn(async move {
       session.run().await;  // 阻塞运行，处理 FUSE 请求
   });
   ```

3. **轮询检查挂载点就绪**：
   ```rust
   for attempt in 0..5 {
       if read_dir(mountpoint).is_ok() {
           return Ok(());  // 挂载成功
       }
       sleep(200ms).await;
   }
   ```

**此时目录结构**：
```
/tmp/antares_build_xxx/
├── mnt/          ← ✅ 已挂载，用户可以看到文件
├── upper/        ← 空（等待用户修改）
├── cl/           ← 空（可选）
└── store/        ← 目录树和文件内容缓存（内存中）
```

---

## 运行时文件分布示例

### 场景 1：用户读取文件（未修改）

**用户操作**：
```bash
cat /tmp/antares_build_xxx/mnt/third-party/buck-hello/BUCK
```

**路由流程**：
1. OverlayFs 检查 upper 层：`upper/third-party/buck-hello/BUCK` ❌ 不存在
2. OverlayFs 检查 CL 层：`cl/third-party/buck-hello/BUCK` ❌ 不存在
3. OverlayFs 检查 Dicfuse 层：✅ 存在（在 store 的内存缓存中）
4. Dicfuse 返回文件内容（从 `store.file_contents[inode]` 读取）

**文件分布**：
```
upper/          ← 空（文件未修改）
cl/             ← 空
store/          ← BUCK 文件内容在内存中（file_contents[inode]）
mnt/            ← 用户看到文件（虚拟视图）
```

### 场景 2：用户修改文件（Copy-up）

**用户操作**：
```bash
echo "new content" > /tmp/antares_build_xxx/mnt/third-party/buck-hello/main.rs
```

**路由流程**：
1. OverlayFs 检查 upper 层：`upper/third-party/buck-hello/main.rs` ❌ 不存在
2. OverlayFs 检查 lower 层：✅ 在 Dicfuse 中存在
3. **触发 Copy-up**：
   - 对可写层（PassthroughFs）获取原始 stat，Dicfuse 只读层不参与 copy-up
   - 在 upper 层创建目录结构：`upper/third-party/buck-hello/`
   - 从 Dicfuse 读取原始内容（可选）
   - 写入 upper 层：`upper/third-party/buck-hello/main.rs`
4. 后续写入直接打在 upper 层

**文件分布**：
```
upper/
└── third-party/
    └── buck-hello/
        └── main.rs          ← ✅ 用户修改的文件（copy-up）

cl/             ← 空

store/          ← main.rs 的原始内容仍在内存中（file_contents[inode]）

mnt/            ← 用户看到修改后的文件（来自 upper）
```

### 场景 3：用户创建新文件

**用户操作**：
```bash
echo "new file" > /tmp/antares_build_xxx/mnt/third-party/buck-hello/new_file.txt
```

**路由流程**：
1. OverlayFs 检查 upper 层：不存在
2. OverlayFs 检查 lower 层：不存在
3. **直接在 upper 层创建**：
   - 创建目录结构：`upper/third-party/buck-hello/`
   - 创建文件：`upper/third-party/buck-hello/new_file.txt`

**文件分布**：
```
upper/
└── third-party/
    └── buck-hello/
        └── new_file.txt     ← ✅ 用户创建的新文件

cl/             ← 空
store/          ← 不包含此文件（新文件）
mnt/            ← 用户看到新文件（来自 upper）
```

### 场景 4：Buck2 构建（失败场景）

**用户操作**：
```bash
cd /tmp/antares_build_xxx/mnt/third-party/buck-hello
buck2 build //...
```

**文件分布**：
```
upper/
└── third-party/
    └── buck-hello/
        └── .buck2/          ← ❌ Buck2 尝试创建 SQLite 文件
            ├── daemon_state.db
            ├── daemon_state.db-shm  ← SQLite SHM 文件（在 FUSE 上失败）
            └── daemon_state.db-wal

cl/             ← 空
store/          ← 原始文件内容缓存
mnt/            ← Buck2 看到的工作目录
```

**问题**：SQLite 的 `.db-shm` 文件需要 `mmap()` 共享内存支持，但 FUSE 不完全支持，导致 Buck2 初始化失败。

---

## 总结

### 目录职责总结表

| 目录 | 作用 | 存储内容 | 可写性 | 用户可见性 |
|------|------|---------|--------|-----------|
| **store/** | Dicfuse 缓存 | 目录树结构、文件内容缓存 | 只读（缓存） | ❌ 不可见 |
| **upper/** | 可写层 | 用户修改/创建的文件 | ✅ 可写 | ❌ 不可见（但内容会反映到 mnt） |
| **cl/** | CL 层（可选） | CL 相关的文件修改 | ✅ 可写 | ❌ 不可见（但内容会反映到 mnt） |
| **mnt/** | 挂载点 | 统一视图（虚拟） | ✅ 可写 | ✅ 用户唯一可见的目录 |

### 关键流程总结

1. **初始化阶段**：
   - 创建 `store/` 目录
   - 创建 Dicfuse 实例
   - 预加载目录树到内存（`import_arc`）

2. **挂载阶段**：
   - 创建 `upper/`、`cl/`、`mnt/` 目录
   - 组装 OverlayFs（upper + CL + Dicfuse）
   - 挂载到 `mnt/` 挂载点

3. **运行时**：
   - 读请求：upper → CL → Dicfuse（按优先级查找）
   - 写请求：必须落在 upper 层（必要时触发 copy-up）
   - 用户只看到 `mnt/` 目录，看不到其他目录的分离

### 文件分布规律

- **只读文件**：只在 `store/` 的内存缓存中，`upper/` 和 `cl/` 为空
- **修改过的文件**：在 `upper/` 中有副本，`store/` 中仍有原始内容
- **新创建的文件**：只在 `upper/` 中，`store/` 中没有
- **用户看到的**：`mnt/` 目录下的统一视图（所有层的合并）

