=== getattr_with_mapping 调用点分析 ===

**在 OverlayFS 中的调用位置:**

- 第 727 行:     /// by using [`getattr_with_mapping`][crate::unionfs::layer::Layer::getattr_with_mapping] and
  上下文:
  ```rust
      /// 2. If not, it first calls itself on its own parent directory.
      /// 3. Once the parent is guaranteed to be in the upper layer, it creates the current
      ///    directory within the parent's upper-layer representation.
      ///
      /// Crucially, it preserves the original directory's ownership (UID/GID) and permissions
      /// by using [`getattr_with_mapping`][crate::unionfs::layer::Layer::getattr_with_mapping] and
      /// [`mkdir_with_context`][crate::unionfs::layer::Layer::mkdir_with_context] with [`OperationContext`][crate::context::OperationContext].
      pub async fn create_upper_dir(
          self: Arc<Self>,
          ctx: Request,
          mode_umask: Option<(u32, u32)>,
  ```

- 第 742 行:             .getattr_with_mapping(self_inode, None, false)
  上下文:
  ```rust
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
  ```

- 第 2106 行:             .getattr_with_mapping(self_inode, None, false)
  上下文:
  ```rust
          // functionalities because `do_getattr_helper` and the standard `stat64()` call
          // both rely on the same underlying `stat` system call; they only differ in
          // whether the resulting `uid` and `gid` are mapped.
          let (self_layer, _, self_inode) = node.first_layer_inode().await;
          let re = self_layer
              .getattr_with_mapping(self_inode, None, false)
              .await?;
          let st = ReplyAttr {
              ttl: re.1,
              attr: convert_stat64_to_file_attr(re.0),
          };
  ```

- 第 2199 行:             .getattr_with_mapping(lower_inode, None, false)
  上下文:
  ```rust
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
  ```

