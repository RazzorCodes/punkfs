//! Direct access to the directory that existed beneath the mount.
//!
//! The root descriptor is opened before mounting. Relative operations use
//! `/proc/self/fd` so they do not traverse the FUSE mount recursively.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::core::backing::{Backing, SafePath};
use crate::store::db::metadata_cache::FsEntry;
use crate::store::{SetattrChanges, StoreApi, StoreChild, StoreError};
use crate::telemetry::{self, Event, OperationContext, Severity};

mod dir;
mod error;
mod helpers;
mod lookup;
mod meta;
mod misc;

pub(crate) struct Local {
    backing: Arc<Backing>,
    node: String,
    mutation: Mutex<()>,
    publisher: Mutex<Option<crate::grpc::peer::Publisher>>,
}

use error::from_io as error;

impl Local {
    pub(crate) fn new(mountpoint: &Path, node: &str) -> io::Result<Arc<Self>> {
        Ok(Self::from_backing(Arc::new(Backing::open(
            mountpoint, node,
        )?)))
    }

    pub(crate) fn from_backing(backing: Arc<Backing>) -> Arc<Self> {
        Arc::new(Self {
            node: backing.node_id().to_owned(),
            backing,
            mutation: Mutex::new(()),
            publisher: Mutex::new(None),
        })
    }

    pub(crate) fn set_publisher(&self, publisher: crate::grpc::peer::Publisher) {
        *self.publisher.lock().unwrap() = Some(publisher);
    }

    fn real(&self, path: &str) -> Result<SafePath, StoreError> {
        helpers::real(self, path)
    }

    fn entry(&self, path: &str) -> Result<FsEntry, StoreError> {
        lookup::entry(self, path)
    }

    fn log(&self, ctx: &OperationContext, operation: &str, result: &Result<(), StoreError>) {
        let Some(logger) = telemetry::global() else {
            return;
        };
        let severity = if result.is_ok() {
            Severity::Debug
        } else {
            Severity::Warning
        };
        if !logger.enabled(severity) {
            return;
        }
        let mut event = Event::new(
            severity,
            "filesystem.passthrough",
            "local filesystem operation completed",
        );
        event.operation = Some(operation.into());
        event.fields.insert(
            "outcome".into(),
            serde_json::json!(if result.is_ok() { "ok" } else { "error" }),
        );
        if let Err(StoreError::Local(errno)) = result {
            event
                .fields
                .insert("errno".into(), serde_json::json!(errno));
        }
        logger.emit_in(ctx, event);
    }

    fn refresh_listing(
        &self,
        store: &dyn StoreApi,
        ctx: &OperationContext,
        dir: &str,
    ) -> Result<(), StoreError> {
        match dir::list(self, ctx, dir) {
            Ok(entries) => store.reconcile_local_in(ctx, dir, entries),
            Err(StoreError::Local(libc::ENOENT)) => store.reconcile_local_in(ctx, dir, Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn ensure_parents(&self, path: &str) -> Result<(), StoreError> {
        self.backing.ensure_parents(path).map_err(error)
    }

    pub(crate) fn readlink(
        &self,
        path: &str,
        ctx: &OperationContext,
    ) -> Result<Vec<u8>, StoreError> {
        let result = self
            .backing
            .entry_path(path)
            .map_err(error)
            .and_then(|real| misc::readlink(&real));
        self.log(
            ctx,
            "readlink",
            &result.as_ref().map(|_| ()).map_err(Clone::clone),
        );
        result
    }

    pub(crate) fn create_symlink(
        &self,
        store: &dyn StoreApi,
        ctx: &OperationContext,
        path: &str,
        target: &Path,
    ) -> Result<u64, StoreError> {
        let result = (|| {
            let _guard = self.mutation.lock().unwrap();
            let parent = path
                .rsplit_once('/')
                .map(|(p, _)| if p.is_empty() { "/" } else { p })
                .unwrap_or("/");
            self.refresh_listing(store, ctx, parent)?;
            if store.get_inode(path).is_some() {
                return Err(StoreError::Local(libc::EEXIST));
            }
            self.ensure_parents(path)?;
            let real = self.backing.entry_path(path).map_err(error)?;
            std::os::unix::fs::symlink(target, &real).map_err(error)?;
            match self
                .entry(path)
                .and_then(|entry| store.upsert_local_in(ctx, entry))
            {
                Ok(inode) => {
                    if let Some(publisher) = self.publisher.lock().unwrap().as_ref() {
                        publisher.created(ctx, path);
                    }
                    Ok(inode)
                }
                Err(err) => {
                    let rollback = fs::remove_file(&real).map_err(error);
                    self.log(ctx, "symlink_rollback", &rollback);
                    Err(err)
                }
            }
        })();
        self.log(
            ctx,
            "symlink",
            &result.as_ref().map(|_| ()).map_err(Clone::clone),
        );
        result
    }

    pub(crate) fn create(
        &self,
        store: &dyn StoreApi,
        ctx: &OperationContext,
        path: &str,
        mode: u32,
        directory: bool,
    ) -> Result<u64, StoreError> {
        let result = (|| {
            let _guard = self.mutation.lock().unwrap();
            let parent = path
                .rsplit_once('/')
                .map(|(p, _)| if p.is_empty() { "/" } else { p })
                .unwrap_or("/");
            // Refuse names known from any node, even if that node is now offline.
            self.refresh_listing(store, ctx, parent)?;
            if store.get_inode(path).is_some() {
                return Err(StoreError::Local(libc::EEXIST));
            }
            self.ensure_parents(path)?;
            let real = self.real(path)?;
            if directory {
                let mut builder = fs::DirBuilder::new();
                builder.mode(mode & 0o7777).create(&real)
            } else {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(mode & 0o7777)
                    .open(&real)
                    .map(|_| ())
            }
            .map_err(error)?;
            match self
                .entry(path)
                .and_then(|entry| store.upsert_local_in(ctx, entry))
            {
                Ok(inode) => {
                    if !directory && let Some(publisher) = self.publisher.lock().unwrap().as_ref() {
                        publisher.created(ctx, path);
                    }
                    Ok(inode)
                }
                Err(e) => {
                    let rollback = if directory {
                        fs::remove_dir(&real)
                    } else {
                        fs::remove_file(&real)
                    }
                    .map_err(error);
                    self.log(ctx, "create_rollback", &rollback);
                    Err(e)
                }
            }
        })();
        self.log(
            ctx,
            if directory { "mkdir" } else { "create" },
            &result.as_ref().map(|_| ()).map_err(Clone::clone),
        );
        result
    }
}

/// An operation-scoped routing facade: the existing FUSE decisions remain
/// shared, while local ownership bypasses gRPC for content and mutations.
pub(crate) struct RoutedStore {
    pub inner: Arc<dyn StoreApi>,
    pub local: Arc<Local>,
    pub ctx: OperationContext,
}

impl RoutedStore {
    fn local(&self, path: &str) -> bool {
        self.inner.owner_of(path).as_deref() == Some(self.local.node.as_str())
    }
    fn check_conflict(&self, path: &str) -> Result<(), StoreError> {
        if self.inner.has_conflict(path) {
            Err(StoreError::Local(libc::EIO))
        } else {
            Ok(())
        }
    }
    fn result<T>(&self, operation: &str, result: Result<T, StoreError>) -> Result<T, StoreError> {
        self.local.log(
            &self.ctx,
            operation,
            &result.as_ref().map(|_| ()).map_err(Clone::clone),
        );
        result
    }
}

impl StoreApi for RoutedStore {
    fn operation_context(&self) -> Option<&OperationContext> {
        Some(&self.ctx)
    }
    fn node_id(&self) -> &str {
        self.inner.node_id()
    }
    fn get_inode(&self, p: &str) -> Option<u64> {
        self.inner.get_inode(p)
    }
    fn get_path(&self, ino: u64) -> Option<String> {
        self.inner.get_path(ino)
    }
    fn get_entry(&self, ino: u64) -> Option<FsEntry> {
        self.inner.get_entry(ino)
    }
    fn owner_of(&self, p: &str) -> Option<String> {
        self.inner.owner_of(p)
    }
    fn has_conflict(&self, p: &str) -> bool {
        self.inner.has_conflict(p)
    }
    fn ensure_listed(&self, p: &str) -> Result<(), StoreError> {
        if self.inner.has_remote_nodes() {
            let remote = self.inner.ensure_listed_in(&self.ctx, p);
            if remote.is_err() {
                self.local.log(&self.ctx, "remote_listing", &remote);
            }
        }
        let local = self
            .local
            .refresh_listing(self.inner.as_ref(), &self.ctx, p);
        self.result("list", local)?;
        // The local directory is an answering node, including an absent
        // local directory when another node supplies the parent.
        Ok(())
    }
    fn get_children(&self, p: &str) -> Vec<StoreChild> {
        let _ = self.ensure_listed(p);
        self.inner.cached_children(p)
    }
    fn readlink(&self, p: &str) -> Result<Vec<u8>, StoreError> {
        self.check_conflict(p)?;
        if self.local(p) {
            self.local.readlink(p, &self.ctx)
        } else {
            self.inner.readlink_in(&self.ctx, p)
        }
    }
    fn read(&self, p: &str, offset: u64, size: u32) -> Result<Vec<u8>, StoreError> {
        self.check_conflict(p)?;
        if !self.local(p) {
            return self.inner.read_in(&self.ctx, p, offset, size);
        }
        let result = self.local.backing.read(p, offset, size).map_err(error);
        self.result("read", result)
    }
    fn write(&self, p: &str, offset: u64, data: &[u8]) -> Result<u32, StoreError> {
        self.check_conflict(p)?;
        if !self.local(p) {
            return self.inner.write_in(&self.ctx, p, offset, data);
        }
        let result = self.local.backing.write(p, offset, data).map_err(error);
        self.result("write", result)
    }
    fn fsync(&self, p: &str, datasync: bool) -> Result<(), StoreError> {
        self.check_conflict(p)?;
        if !self.local(p) {
            return self.inner.fsync_in(&self.ctx, p, datasync);
        }
        let result = self.local.backing.fsync(p, datasync).map_err(error);
        self.result("fsync", result)
    }
    fn setattr(
        &self,
        p: &str,
        c: SetattrChanges,
    ) -> Result<crate::store::db::attributes::FileAttrData, StoreError> {
        self.check_conflict(p)?;
        if !self.local(p) {
            return self.inner.setattr_in(&self.ctx, p, c);
        }
        let result = (|| {
            let path = self.local.backing.existing(p).map_err(error)?;
            meta::setattr(&path, c)?;
            let entry = self.local.entry(p)?;
            let attr = entry.attr.clone();
            self.inner.upsert_local_in(&self.ctx, entry)?;
            Ok(attr)
        })();
        self.result("setattr", result)
    }
    fn refresh(&self, p: &str) -> Result<crate::store::db::attributes::FileAttrData, StoreError> {
        self.check_conflict(p)?;
        if !self.local(p) {
            return self.inner.refresh_in(&self.ctx, p);
        }
        let result = (|| {
            let entry = self.local.entry(p)?;
            let attr = entry.attr.clone();
            self.inner.upsert_local_in(&self.ctx, entry)?;
            Ok(attr)
        })();
        self.result("getattr", result)
    }
    fn statfs(&self) -> Result<crate::store::StatFs, StoreError> {
        self.result("statfs", meta::statfs(&self.local.real("/")?))
    }
    fn unlink(&self, p: &str) -> Result<(), StoreError> {
        self.check_conflict(p)?;
        if !self.local(p) {
            // Regular-file replacement needs transactional handling. A symlink
            // can be removed directly without touching its target.
            let symlink = self
                .inner
                .get_inode(p)
                .and_then(|ino| self.inner.get_entry(ino))
                .is_some_and(|entry| entry.kind == crate::store::db::attributes::FsEntryKind::Link);
            if symlink {
                return self.inner.unlink_in(&self.ctx, p);
            }
            return self.result("unlink", Err(StoreError::NotPermitted));
        }
        let result = (|| {
            let _guard = self.local.mutation.lock().unwrap();
            fs::remove_file(self.local.backing.entry_path(p).map_err(error)?).map_err(error)?;
            self.inner.remove_local_in(&self.ctx, p)
        })();
        self.result("unlink", result)
    }
    fn rmdir(&self, p: &str) -> Result<(), StoreError> {
        self.check_conflict(p)?;
        if !self.local(p) {
            return self.inner.rmdir_in(&self.ctx, p);
        }
        let result = (|| {
            let _guard = self.local.mutation.lock().unwrap();
            fs::remove_dir(self.local.real(p)?).map_err(error)?;
            self.inner.remove_local_in(&self.ctx, p)
        })();
        self.result("rmdir", result)
    }
    fn rename(&self, old: &str, new: &str, flags: fuser::RenameFlags) -> Result<(), StoreError> {
        self.check_conflict(old)?;
        self.check_conflict(new)?;
        if !self.local(old) {
            if old != new && self.inner.owner_of(new).as_deref() == Some(self.local.node.as_str()) {
                return self.result("rename", Err(StoreError::Local(libc::EXDEV)));
            }
            return self.inner.rename_in(&self.ctx, old, new, flags);
        }
        if old != new
            && self
                .inner
                .owner_of(new)
                .is_some_and(|owner| owner != self.local.node)
        {
            return self.result("rename", Err(StoreError::Local(libc::EXDEV)));
        }
        if flags.contains(fuser::RenameFlags::RENAME_EXCHANGE) {
            return self.result(
                "rename",
                Err(StoreError::Unsupported(
                    "local exchange rename is not supported".into(),
                )),
            );
        }
        let result = (|| {
            let _guard = self.local.mutation.lock().unwrap();
            self.local.ensure_parents(new)?;
            meta::rename(
                &self.local.backing.entry_path(old).map_err(error)?,
                &self.local.backing.entry_path(new).map_err(error)?,
                flags,
            )?;
            self.inner.rename_local_in(&self.ctx, old, new)
        })();
        self.result("rename", result)
    }
}
