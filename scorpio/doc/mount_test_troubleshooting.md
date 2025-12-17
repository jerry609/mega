# Mount Test 问题排查和解决方案

## 问题现象

程序在加载目录树时卡住，输出停止在 `max_depth reach path` 消息之后。

## 问题分析

### 1. 程序没有真正"卡住"

- 进程仍在运行（CPU 使用率高）
- 正在从远端 API 拉取目录树
- 大仓库可能需要 10-30 分钟才能完成

### 2. 可能的原因

1. **加载深度过大**：`load_dir_depth=3` 意味着实际加载 5 层深度
2. **深层目录子项多**：`/third-party/mega/moon/` 等目录下有很多子目录
3. **网络请求累积**：每个目录需要一次网络请求，大量目录会累积耗时
4. **Worker 等待**：所有 worker 在等待 `join_all(workers).await` 完成

### 3. 已修复的问题

- 在 `store.rs` 中改进了错误处理，确保即使 `fetch_dir` 失败也会正确减少 producer 计数

## 解决方案

### 方案 1：使用减少深度的配置（推荐）

```bash
# 使用快速测试配置（load_dir_depth=2）
sudo -E cargo run -p scorpio --bin mount_test -- \
    --config-path scorpio/scorpio_test.toml \
    --keep-alive 300
```

### 方案 2：等待当前进程完成

如果进程仍在运行（CPU 使用率高），可以等待它完成：
- 查看进程状态：`ps aux | grep mount_test`
- 如果 CPU > 100%，说明正在工作，可以等待
- 通常需要 10-30 分钟，取决于仓库大小

### 方案 3：终止并重新运行

```bash
# 终止所有 mount_test 进程
sudo pkill -f mount_test

# 使用减少深度的配置重新运行
sudo -E cargo run -p scorpio --bin mount_test -- \
    --config-path scorpio/scorpio_test.toml \
    --keep-alive 300
```

## 验证挂载是否成功

```bash
# 查找挂载点
MOUNT_POINT=$(find /tmp -maxdepth 2 -type d -path "*/antares_test_*/mnt" 2>/dev/null | head -1)

# 检查挂载点
if [ -n "$MOUNT_POINT" ]; then
    echo "挂载点: $MOUNT_POINT"
    ls -la "$MOUNT_POINT"
    mountpoint "$MOUNT_POINT"
fi
```

## 性能优化建议

1. **减少加载深度**：根据实际需求设置 `load_dir_depth`（通常 2-3 足够）
2. **使用缓存**：如果之前运行过，Dicfuse 会使用缓存，速度会快很多
3. **避免多进程**：同时只运行一个 `mount_test` 进程，避免资源竞争

## 调试技巧

1. **查看实时日志**：
   ```bash
   # 如果使用重定向输出
   tail -f /tmp/mount_test_output.log
   ```

2. **检查进程状态**：
   ```bash
   ps aux | grep mount_test
   # CPU 使用率高 = 正在工作
   # CPU 使用率 0 = 可能卡住
   ```

3. **检查网络连接**：
   ```bash
   netstat -an | grep ":80 " | grep ESTABLISHED
   ```

4. **检查挂载点**：
   ```bash
   mount | grep antares
   find /tmp -maxdepth 2 -type d -name "antares_test_*"
   ```

