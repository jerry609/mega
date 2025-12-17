=== Layer trait: getattr_with_mapping 方法签名 ===

**0.1.9 版本** (libfuse-fs-0.1.9/src/unionfs/layer.rs):
```rust
    async fn getattr_with_mapping(
        &self,
        _inode: Inode,
        _handle: Option<u64>,
        _mapping: bool,
    ) -> std::io::Result<(libc::stat64, Duration)> {
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
    }
}

#[async_trait]
--
```

**0.1.8 版本** (libfuse-fs-0.1.8/src/unionfs/layer.rs):
```rust
    async fn do_getattr_helper(
        &self,
        _inode: Inode,
        _handle: Option<u64>,
    ) -> std::io::Result<(libc::stat64, Duration)> {
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
    }
}
#[async_trait]
impl Layer for PassthroughFs {
    fn root_inode(&self) -> Inode {
--
```

