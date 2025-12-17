=== OverlayFS::copy_regfile_up 方法 ===

**0.1.9 版本** (libfuse-fs-0.1.9/src/unionfs/mod.rs):

方法位置: 第 2176 行

```rust
    async fn copy_regfile_up(
        &self,
        ctx: Request,
        node: Arc<OverlayInode>,
    ) -> Result<Arc<OverlayInode>> {
        if node.in_upper_layer().await {
            return Ok(node);
        }

        let parent_node = if let Some(ref n) = node.parent.lock().await.upgrade() {
            Arc::clone(n)
        } else {
            return Err(Error::other("no parent?"));
        };

        // To preserve original ownership, we must get the raw, unmapped host attributes.
        // We achieve this by calling `do_getattr_helper`, which is specifically designed
        // to bypass the ID mapping logic. This is safe and does not affect other
        // functionalities because `do_getattr_helper` and the standard `stat64()` call
        // both rely on the same underlying `stat` system call; they only differ in
        // whether the resulting `uid` and `gid` are mapped.
        let (lower_layer, _, lower_inode) = node.first_layer_inode().await;
        let re = lower_layer
            .getattr_with_mapping(lower_inode, None, false)
            .await?;
        let st = ReplyAttr {
            ttl: re.1,
            attr: convert_stat64_to_file_attr(re.0),
        };
        trace!(
            "copy_regfile_up: node {} in lower layer's inode {}",
            node.inode, lower_inode
        );

        if !parent_node.in_upper_layer().await {
            parent_node.clone().create_upper_dir(ctx, None).await?;
        }

        // create the file in upper layer using information from lower layer

```

=== OverlayFS::create_upper_dir 方法 ===

方法位置: 第 729 行

```rust
    pub async fn create_upper_dir(
        self: Arc<Self>,
        ctx: Request,
        mode_umask: Option<(u32, u32)>,
    ) -> Result<()> {
        // To preserve original ownership, we must get the raw, unmapped host attributes.
        // We achieve this by calling `do_getattr_helper`, which is specifically designed
        // to bypass the ID mapping logic. This is safe and does not affect other
        // functionalities because `do_getattr_helper` and the standard `stat64()` call
        // both rely on the same underlying `stat` system call; they only differ in
        // whether the resulting `uid` and `gid` are mapped.
        let (self_layer, _, self_inode) = self.first_layer_inode().await;
        let re = self_layer
            .getattr_with_mapping(self_inode, None, false)
            .await?;
        let st = ReplyAttr {
            ttl: re.1,
            attr: convert_stat64_to_file_attr(re.0),
        };
        if !utils::is_dir(&st.attr.kind) {
            return Err(Error::from_raw_os_error(libc::ENOTDIR));
        }

        // If node already has upper layer, we can just return here.
        if self.in_upper_layer().await {
```
