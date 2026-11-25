//! Standalone functional calls are ingress operations: each gets a fresh context.
//! Correlation-specific tests use `StoreApi` directly with explicit contexts.
use punkfs::core::types::FileAttrData;
use punkfs::store::{SetattrChanges, StatFs, Store, StoreApi, StoreChild, StoreError};
use punkfs::telemetry::OperationContext;

pub trait TestStoreApi {
    fn ensure_listed(&self, path: &str) -> Result<(), StoreError>;
    fn get_children(&self, path: &str) -> Vec<StoreChild>;
    fn readlink(&self, path: &str) -> Result<Vec<u8>, StoreError>;
    fn read(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>, StoreError>;
    fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<u32, StoreError>;
    fn fsync(&self, path: &str, datasync: bool) -> Result<(), StoreError>;
    fn get_xattr(&self, path: &str, name: &[u8]) -> Result<Vec<u8>, StoreError>;
    fn list_xattr(&self, path: &str) -> Result<Vec<u8>, StoreError>;
    fn set_xattr(
        &self,
        path: &str,
        name: &[u8],
        value: &[u8],
        flags: i32,
    ) -> Result<(), StoreError>;
    fn remove_xattr(&self, path: &str, name: &[u8]) -> Result<(), StoreError>;
    fn setattr(&self, path: &str, changes: SetattrChanges) -> Result<FileAttrData, StoreError>;
    fn refresh(&self, path: &str) -> Result<FileAttrData, StoreError>;
    fn statfs(&self) -> Result<StatFs, StoreError>;
    fn unlink(&self, path: &str) -> Result<(), StoreError>;
    fn rmdir(&self, path: &str) -> Result<(), StoreError>;
    fn rename(&self, old: &str, new: &str, flags: fuser::RenameFlags) -> Result<(), StoreError>;
}
impl TestStoreApi for Store {
    fn ensure_listed(&self, p: &str) -> Result<(), StoreError> {
        self.ensure_listed_in(&OperationContext::new(), p)
    }
    fn get_children(&self, p: &str) -> Vec<StoreChild> {
        self.get_children_in(&OperationContext::new(), p)
    }
    fn readlink(&self, p: &str) -> Result<Vec<u8>, StoreError> {
        self.readlink_in(&OperationContext::new(), p)
    }
    fn read(&self, p: &str, o: u64, s: u32) -> Result<Vec<u8>, StoreError> {
        self.read_in(&OperationContext::new(), p, o, s)
    }
    fn write(&self, p: &str, o: u64, d: &[u8]) -> Result<u32, StoreError> {
        self.write_in(&OperationContext::new(), p, o, d)
    }
    fn fsync(&self, p: &str, d: bool) -> Result<(), StoreError> {
        self.fsync_in(&OperationContext::new(), p, d)
    }
    fn get_xattr(&self, p: &str, n: &[u8]) -> Result<Vec<u8>, StoreError> {
        self.get_xattr_in(&OperationContext::new(), p, n)
    }
    fn list_xattr(&self, p: &str) -> Result<Vec<u8>, StoreError> {
        self.list_xattr_in(&OperationContext::new(), p)
    }
    fn set_xattr(&self, p: &str, n: &[u8], v: &[u8], f: i32) -> Result<(), StoreError> {
        self.set_xattr_in(&OperationContext::new(), p, n, v, f)
    }
    fn remove_xattr(&self, p: &str, n: &[u8]) -> Result<(), StoreError> {
        self.remove_xattr_in(&OperationContext::new(), p, n)
    }
    fn setattr(&self, p: &str, c: SetattrChanges) -> Result<FileAttrData, StoreError> {
        self.setattr_in(&OperationContext::new(), p, c)
    }
    fn refresh(&self, p: &str) -> Result<FileAttrData, StoreError> {
        self.refresh_in(&OperationContext::new(), p)
    }
    fn statfs(&self) -> Result<StatFs, StoreError> {
        self.statfs_in(&OperationContext::new())
    }
    fn unlink(&self, p: &str) -> Result<(), StoreError> {
        self.unlink_in(&OperationContext::new(), p)
    }
    fn rmdir(&self, p: &str) -> Result<(), StoreError> {
        self.rmdir_in(&OperationContext::new(), p)
    }
    fn rename(&self, a: &str, b: &str, f: fuser::RenameFlags) -> Result<(), StoreError> {
        self.rename_in(&OperationContext::new(), a, b, f)
    }
}
