//! Bind an operation context to the store without global or thread-local state.
use super::{SetattrChanges, StatFs, StoreApi, StoreChild, StoreError};
use crate::core::types::FileAttrData;
use crate::core::metadata::FsEntry;
use crate::telemetry::OperationContext;
use fuser::RenameFlags;

pub(crate) struct ContextualStore {
    pub inner: std::sync::Arc<dyn StoreApi>,
    pub context: OperationContext,
}

impl StoreApi for ContextualStore {
    fn operation_context(&self) -> Option<&OperationContext> {
        Some(&self.context)
    }
    fn node_id(&self) -> &str {
        self.inner.node_id()
    }
    fn get_inode(&self, path: &str) -> Option<u64> {
        self.inner.get_inode(path)
    }
    fn get_path(&self, inode: u64) -> Option<String> {
        self.inner.get_path(inode)
    }
    fn get_entry(&self, inode: u64) -> Option<FsEntry> {
        self.inner.get_entry(inode)
    }
    fn owner_of(&self, path: &str) -> Option<String> {
        self.inner.owner_of(path)
    }
    fn ensure_listed(&self, dir: &str) -> Result<(), StoreError> {
        self.inner.ensure_listed_in(&self.context, dir)
    }
    fn get_children(&self, path: &str) -> Vec<StoreChild> {
        self.inner.get_children_in(&self.context, path)
    }
    fn readlink(&self, path: &str) -> Result<Vec<u8>, StoreError> {
        self.inner.readlink_in(&self.context, path)
    }
    fn read(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>, StoreError> {
        self.inner.read_in(&self.context, path, offset, size)
    }
    fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<u32, StoreError> {
        self.inner.write_in(&self.context, path, offset, data)
    }
    fn fsync(&self, path: &str, datasync: bool) -> Result<(), StoreError> {
        self.inner.fsync_in(&self.context, path, datasync)
    }
    fn get_xattr(&self, p: &str, n: &[u8]) -> Result<Vec<u8>, StoreError> {
        self.inner.get_xattr_in(&self.context, p, n)
    }
    fn list_xattr(&self, p: &str) -> Result<Vec<u8>, StoreError> {
        self.inner.list_xattr_in(&self.context, p)
    }
    fn set_xattr(&self, p: &str, n: &[u8], v: &[u8], f: i32) -> Result<(), StoreError> {
        self.inner.set_xattr_in(&self.context, p, n, v, f)
    }
    fn remove_xattr(&self, p: &str, n: &[u8]) -> Result<(), StoreError> {
        self.inner.remove_xattr_in(&self.context, p, n)
    }
    fn setattr(&self, path: &str, changes: SetattrChanges) -> Result<FileAttrData, StoreError> {
        self.inner.setattr_in(&self.context, path, changes)
    }
    fn refresh(&self, path: &str) -> Result<FileAttrData, StoreError> {
        self.inner.refresh_in(&self.context, path)
    }
    fn statfs(&self) -> Result<StatFs, StoreError> {
        self.inner.statfs_in(&self.context)
    }
    fn unlink(&self, path: &str) -> Result<(), StoreError> {
        self.inner.unlink_in(&self.context, path)
    }
    fn rmdir(&self, path: &str) -> Result<(), StoreError> {
        self.inner.rmdir_in(&self.context, path)
    }
    fn rename(&self, old: &str, new: &str, flags: RenameFlags) -> Result<(), StoreError> {
        self.inner.rename_in(&self.context, old, new, flags)
    }
}
