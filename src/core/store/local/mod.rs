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
use crate::core::types::FsEntry;
use crate::core::types::{FileAttrData, misc::xattr};
use crate::store::{SetattrChanges, StatFs, Store, StoreApi, StoreError};
use crate::telemetry::{self, Event, OperationContext, Severity};

mod dir;
mod error;
mod helpers;
mod lookup;
mod meta;
mod misc;

pub(crate) struct Local {
    backing: Arc<Backing>,
    mutation: Mutex<()>,
    publisher: Mutex<Option<crate::grpc::peer::Publisher>>,
}

use error::from_io as error;

impl Local {
    pub(crate) fn from_backing(backing: Arc<Backing>) -> Arc<Self> {
        Arc::new(Self {
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

    pub(super) fn log(
        &self,
        ctx: &OperationContext,
        operation: &str,
        result: &Result<(), StoreError>,
    ) {
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
        store: &Store,
        ctx: &OperationContext,
        dir: &str,
    ) -> Result<(), StoreError> {
        let generation = store.local_generation();
        let entries = match dir::list(self, ctx, dir) {
            Ok(entries) => entries,
            Err(StoreError::Local(libc::ENOENT)) => Vec::new(),
            Err(e) => return Err(e),
        };
        store.reconcile_local_listing(ctx, dir, entries, generation)
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
        store: &Store,
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
            if store.catalog_path_in(ctx, path)?.is_some() {
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

    pub(crate) fn create_special(
        &self,
        store: &Store,
        ctx: &OperationContext,
        path: &str,
        mode: u32,
        rdev: u32,
    ) -> Result<u64, StoreError> {
        use std::os::unix::ffi::OsStrExt;
        let result = (|| {
            let _guard = self.mutation.lock().unwrap();
            let parent = path
                .rsplit_once('/')
                .map(|(p, _)| if p.is_empty() { "/" } else { p })
                .unwrap_or("/");
            self.refresh_listing(store, ctx, parent)?;
            if store.catalog_path_in(ctx, path)?.is_some() {
                return Err(StoreError::Local(libc::EEXIST));
            }
            self.ensure_parents(path)?;
            let real = self.backing.entry_path(path).map_err(error)?;
            let name = std::ffi::CString::new(real.as_os_str().as_bytes())
                .map_err(|_| StoreError::Local(libc::EINVAL))?;
            if unsafe { libc::mknod(name.as_ptr(), mode as libc::mode_t, rdev as libc::dev_t) } != 0
            {
                return Err(error(io::Error::last_os_error()));
            }
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
                    self.log(ctx, "mknod_rollback", &rollback);
                    Err(err)
                }
            }
        })();
        self.log(
            ctx,
            "mknod",
            &result.as_ref().map(|_| ()).map_err(Clone::clone),
        );
        result
    }

    pub(crate) fn create(
        &self,
        store: &Store,
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
            if store.catalog_path_in(ctx, path)?.is_some() {
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

    pub(super) fn list_into(
        &self,
        store: &Store,
        ctx: &OperationContext,
        path: &str,
    ) -> Result<(), StoreError> {
        let _guard = self.mutation.lock().unwrap();
        self.logged(ctx, "list", self.refresh_listing(store, ctx, path))
    }

    fn logged<T>(
        &self,
        ctx: &OperationContext,
        op: &str,
        result: Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.log(ctx, op, &result.as_ref().map(|_| ()).map_err(Clone::clone));
        result
    }

    pub(super) fn read(
        &self,
        ctx: &OperationContext,
        path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, StoreError> {
        self.logged(
            ctx,
            "read",
            self.backing.read(path, offset, size).map_err(error),
        )
    }

    pub(super) fn write(
        &self,
        ctx: &OperationContext,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<u32, StoreError> {
        self.logged(
            ctx,
            "write",
            self.backing.write(path, offset, data).map_err(error),
        )
    }

    pub(super) fn fsync(
        &self,
        ctx: &OperationContext,
        path: &str,
        datasync: bool,
    ) -> Result<(), StoreError> {
        self.logged(
            ctx,
            "fsync",
            self.backing.fsync(path, datasync).map_err(error),
        )
    }

    fn xattr<T>(
        &self,
        ctx: &OperationContext,
        op: &str,
        path: &str,
        f: impl FnOnce(&Path) -> io::Result<T>,
    ) -> Result<T, StoreError> {
        let result = self
            .backing
            .entry_path(path)
            .map_err(error)
            .and_then(|real| f(&real).map_err(error));
        self.logged(ctx, op, result)
    }

    pub(super) fn get_xattr(
        &self,
        ctx: &OperationContext,
        path: &str,
        name: &[u8],
    ) -> Result<Vec<u8>, StoreError> {
        self.xattr(ctx, "getxattr", path, |real| xattr::get(real, name))
    }

    pub(super) fn list_xattr(
        &self,
        ctx: &OperationContext,
        path: &str,
    ) -> Result<Vec<u8>, StoreError> {
        self.xattr(ctx, "listxattr", path, xattr::list)
    }

    pub(super) fn set_xattr(
        &self,
        ctx: &OperationContext,
        path: &str,
        name: &[u8],
        value: &[u8],
        flags: i32,
    ) -> Result<(), StoreError> {
        self.xattr(ctx, "setxattr", path, |real| {
            xattr::set(real, name, value, flags)
        })
    }

    pub(super) fn remove_xattr(
        &self,
        ctx: &OperationContext,
        path: &str,
        name: &[u8],
    ) -> Result<(), StoreError> {
        self.xattr(ctx, "removexattr", path, |real| xattr::remove(real, name))
    }

    fn refresh_entry(
        &self,
        store: &Store,
        ctx: &OperationContext,
        path: &str,
    ) -> Result<FileAttrData, StoreError> {
        let expected = store.catalog_path_in(ctx, path)?;
        let attr = self.entry(path)?.attr;
        store.store_attr(
            ctx,
            path,
            expected.as_ref(),
            self.backing.node_id(),
            attr.clone(),
        )?;
        Ok(attr)
    }

    pub(super) fn setattr(
        &self,
        store: &Store,
        ctx: &OperationContext,
        path: &str,
        changes: SetattrChanges,
    ) -> Result<FileAttrData, StoreError> {
        let _guard = self.mutation.lock().unwrap();
        let result = (|| {
            meta::setattr(&self.backing.existing(path).map_err(error)?, changes)?;
            self.refresh_entry(store, ctx, path)
        })();
        self.logged(ctx, "setattr", result)
    }

    pub(super) fn refresh(
        &self,
        store: &Store,
        ctx: &OperationContext,
        path: &str,
    ) -> Result<FileAttrData, StoreError> {
        let _guard = self.mutation.lock().unwrap();
        self.logged(ctx, "getattr", self.refresh_entry(store, ctx, path))
    }

    pub(super) fn statfs(&self, ctx: &OperationContext) -> Result<StatFs, StoreError> {
        self.logged(
            ctx,
            "statfs",
            self.real("/").and_then(|root| meta::statfs(&root)),
        )
    }

    pub(super) fn unlink(
        &self,
        store: &Store,
        ctx: &OperationContext,
        path: &str,
    ) -> Result<(), StoreError> {
        let _guard = self.mutation.lock().unwrap();
        let result = (|| {
            fs::remove_file(self.backing.entry_path(path).map_err(error)?).map_err(error)?;
            store.remove_local_in(ctx, path)
        })();
        self.logged(ctx, "unlink", result)
    }

    pub(super) fn rmdir(
        &self,
        store: &Store,
        ctx: &OperationContext,
        path: &str,
    ) -> Result<(), StoreError> {
        let _guard = self.mutation.lock().unwrap();
        let result = (|| {
            fs::remove_dir(self.real(path)?).map_err(error)?;
            store.remove_local_in(ctx, path)
        })();
        self.logged(ctx, "rmdir", result)
    }

    pub(super) fn rename(
        &self,
        store: &Store,
        ctx: &OperationContext,
        old: &str,
        new: &str,
        flags: fuser::RenameFlags,
    ) -> Result<(), StoreError> {
        let _guard = self.mutation.lock().unwrap();
        let result = (|| {
            self.ensure_parents(new)?;
            meta::rename(
                &self.backing.entry_path(old).map_err(error)?,
                &self.backing.entry_path(new).map_err(error)?,
                flags,
            )?;
            store.update_local_rename_in(ctx, old, new, flags)
        })();
        self.logged(ctx, "rename", result)
    }
}
