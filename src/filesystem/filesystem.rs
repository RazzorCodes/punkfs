//! FUSE adapter. Request handling delegates backing operations to the store.

#[cfg(test)]
use crate::filesystem::ops::helpers;
use crate::filesystem::ops::{dir, file, lookup, meta, misc};

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use fuser::FileHandle;
use fuser::Filesystem;
use fuser::INodeNo;
use fuser::MountOption;
use fuser::ReplyAttr;
use fuser::Request;

use crate::store::{CreateEntry, StoreApi};

/// The `fs` itself: implements the fuser `Filesystem` trait below.
///
/// The store owns the catalog and backing routing. Each callback uses an
/// operation-local view with a fresh correlation context, passed explicitly
/// to the operation helpers and store methods. Tests can inject a mock store.
pub struct Fs {
    /// Shared store; callbacks never bind their context into the store.
    pub(crate) store: Arc<dyn StoreApi>,
    pub(crate) context: crate::telemetry::OperationContext,
    route_console: bool,
}

/// FUSE mount configuration. Construct the store before mounting so its
/// local backing, if any, is pinned before the kernel covers the directory.
pub struct MountOptions {
    pub store: Arc<dyn StoreApi>,
    /// Opt-in console diagnostics. These include file and directory names.
    pub route_console: bool,
}

impl MountOptions {
    pub fn new(store: Arc<dyn StoreApi>) -> Self {
        Self {
            store,
            route_console: false,
        }
    }
}

impl Fs {
    /// Bind a fresh ingress identity to this FUSE callback and all its store work.
    fn for_operation(&self) -> Self {
        Self {
            store: self.store.clone(),
            context: crate::telemetry::OperationContext::new(),
            route_console: self.route_console,
        }
    }

    /// Report the selected cached owner; directories are labeled as merged
    /// because their namespace can have multiple contributors.
    pub(crate) fn log_route(&self, operation: &str, inode: u64) {
        if !self.route_console {
            return;
        }
        let Ok(Some(entry)) = self.store.catalog_inode_in(&self.context, inode) else {
            return;
        };
        let ctx = &self.context;
        let Some(logger) = crate::telemetry::global() else {
            return;
        };
        if !logger.enabled(crate::telemetry::Severity::Info) {
            return;
        }
        let conflict = self.store.has_conflict(&entry.path);
        let directory = entry.kind == crate::core::types::FsEntryKind::Directory;
        let (owner, location) = if conflict {
            ("unresolved", "conflict")
        } else if directory || entry.node_id.is_empty() {
            ("multiple", "merged")
        } else if self.store.has_local_backing() && entry.node_id == self.store.node_id() {
            (entry.node_id.as_str(), "local")
        } else {
            (entry.node_id.as_str(), "remote")
        };
        let message = format!(
            "{operation} {:?}: node={owner:?} location={location}{}",
            entry.path,
            if directory {
                " (directory; may merge nodes)"
            } else {
                ""
            }
        );
        let mut event = crate::telemetry::Event::new(
            crate::telemetry::Severity::Info,
            "filesystem.route",
            message,
        );
        event.operation = Some(operation.into());
        event.console = true;
        event
            .fields
            .insert("path".into(), serde_json::json!(entry.path));
        event
            .fields
            .insert("node_id".into(), serde_json::json!(owner));
        event
            .fields
            .insert("location".into(), serde_json::json!(location));

        logger.emit_in(ctx, event);
    }

    fn log_child_route(&self, operation: &str, parent: u64, name: &std::ffi::OsStr) {
        if !self.route_console {
            return;
        }
        if let Ok(parent) =
            crate::filesystem::ops::helpers::path_of(self.store.as_ref(), &self.context, parent)
            && let Ok(path) = crate::filesystem::ops::helpers::child_path(&parent, name)
            && let Ok(Some(entry)) = self.store.catalog_path_in(&self.context, &path)
        {
            self.log_route(operation, entry.inode);
        }
    }

    /// Build an `Fs` over an arbitrary store implementation.
    ///
    /// Used by the unit tests (with `crate::store::mock::MockStore`) to
    /// drive the op modules against a catalog with no server behind it.
    #[cfg(test)]
    pub(crate) fn with_store(store: Box<dyn StoreApi>) -> Self {
        Fs {
            store: Arc::from(store),
            context: crate::telemetry::OperationContext::new(),
            route_console: false,
        }
    }

    /// Attributes of `inode`, as served to the kernel.
    #[cfg(test)]
    pub(crate) fn attr_for(&self, inode: u64, uid: u32, gid: u32) -> Option<fuser::FileAttr> {
        helpers::attr_for(self.store.as_ref(), inode, uid, gid)
    }

    /// Mount a configured store. The returned session owns the FUSE mount.
    pub fn mount(
        mountpoint: &Path,
        options: MountOptions,
    ) -> Result<fuser::BackgroundSession, Box<dyn std::error::Error>> {
        let fs = Self {
            store: options.store,
            context: crate::telemetry::OperationContext::new(),
            route_console: options.route_console,
        };
        let mut cfg = fuser::Config::default();
        cfg.mount_options = vec![MountOption::FSName("punkfs".into())];
        Ok(fuser::spawn_mount(fs, mountpoint, &cfg)?)
    }
}

/// The single `impl Filesystem for Fs`. Every method delegates to the free
/// function of the same name in its op module.
impl Filesystem for Fs {
    // lookup.rs
    /// Resolve `name` (a child of directory `parent`) to an inode and its
    /// attrs. Triggered by every stat-like call and as the first half of
    /// any path-based access. Delegates to `lookup::lookup`.
    fn lookup(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        reply: fuser::ReplyEntry,
    ) {
        let operation = self.for_operation();
        lookup::lookup(&operation, req, parent, name, reply);
    }

    /// Answer with the attributes of `ino` (stat). Delegates to
    /// `lookup::getattr`.
    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let operation = self.for_operation();
        lookup::getattr(&operation, req, ino, fh, reply);
    }

    // dir.rs
    /// Begin a directory read session; `fh` returned is passed to later
    /// `readdir` calls. Delegates to `dir::opendir`.
    fn opendir(
        &self,
        req: &Request,
        ino: INodeNo,
        flags: fuser::OpenFlags,
        reply: fuser::ReplyOpen,
    ) {
        let operation = self.for_operation();
        dir::opendir(&operation, req, ino, flags, reply);
    }

    /// List the contents of the directory `ino`, starting at `offset`
    /// (an index previously returned by `reply.add`; the kernel calls us
    /// repeatedly until we send an empty batch). Delegates to
    /// `dir::readdir`.
    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        reply: fuser::ReplyDirectory,
    ) {
        let operation = self.for_operation();
        dir::readdir(&operation, req, ino, fh, offset, reply);
    }

    // file.rs
    /// Open a file; the returned `fh` is passed to `read`/`write`/etc.
    /// Delegates to `file::open`.
    fn open(&self, req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: fuser::ReplyOpen) {
        let operation = self.for_operation();
        operation.log_route("open", ino.0);
        file::open(&operation, req, ino, flags, reply);
    }

    /// Read up to `size` bytes of file `ino` starting at byte `offset`.
    /// Delegates to `file::read`.
    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        flags: fuser::OpenFlags,
        lock_owner: Option<fuser::LockOwner>,
        reply: fuser::ReplyData,
    ) {
        let operation = self.for_operation();
        operation.log_route("read", ino.0);
        file::read(
            &operation, req, ino, fh, offset, size, flags, lock_owner, reply,
        );
    }

    /// Write `data` into file `ino` at byte `offset`. Delegates to
    /// `file::write`.
    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: fuser::WriteFlags,
        flags: fuser::OpenFlags,
        lock_owner: Option<fuser::LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        let operation = self.for_operation();
        operation.log_route("write", ino.0);
        file::write(
            &operation,
            req,
            ino,
            fh,
            offset,
            data,
            write_flags,
            flags,
            lock_owner,
            reply,
        );
    }

    /// Called on every `close()` of an open file (may be several times
    /// per `open` due to `dup`/fork). Delegates to `file::flush`.
    fn flush(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: fuser::LockOwner,
        reply: fuser::ReplyEmpty,
    ) {
        let operation = self.for_operation();
        file::flush(&operation, req, ino, fh, lock_owner, reply);
    }

    /// Called exactly once per `open`, when the last reference to the
    /// file goes away. Delegates to `file::release`.
    fn release(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        flags: fuser::OpenFlags,
        lock_owner: Option<fuser::LockOwner>,
        flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        let operation = self.for_operation();
        file::release(&operation, req, ino, fh, flags, lock_owner, flush, reply);
    }

    /// Flush file contents to stable storage (`fsync`/`fdatasync`):
    /// `datasync` true means skip metadata. Delegates to `file::fsync`.
    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        let operation = self.for_operation();
        operation.log_route("fsync", ino.0);
        file::fsync(&operation, req, ino, fh, datasync, reply);
    }

    // meta.rs
    /// Change attributes of `ino` — chmod/chown/truncate/utimens all
    /// land here; whichever `Option`s are `Some` are the requested
    /// changes. Delegates to `meta::setattr`.
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let operation = self.for_operation();
        operation.log_route("setattr", ino.0);
        meta::setattr(
            &operation, req, ino, mode, uid, gid, size, atime, mtime, ctime, fh, crtime, chgtime,
            bkuptime, flags, reply,
        );
    }

    /// Remove the file `name` inside `parent`. Delegates to
    /// `meta::unlink`.
    fn unlink(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        let operation = self.for_operation();
        operation.log_child_route("unlink", parent.0, name);
        meta::unlink(&operation, req, parent, name, reply);
    }

    /// Remove the empty directory `name` inside `parent`. Delegates to
    /// `meta::rmdir`.
    fn rmdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        let operation = self.for_operation();
        operation.log_child_route("rmdir", parent.0, name);
        meta::rmdir(&operation, req, parent, name, reply);
    }

    /// Rename/move `parent/name` to `newparent/newname` (`mv`).
    /// Delegates to `meta::rename`.
    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        newparent: INodeNo,
        newname: &std::ffi::OsStr,
        flags: fuser::RenameFlags,
        reply: fuser::ReplyEmpty,
    ) {
        let operation = self.for_operation();
        operation.log_child_route("rename", parent.0, name);
        meta::rename(
            &operation, req, parent, name, newparent, newname, flags, reply,
        );
    }

    /// Report the filesystem's overall stats (free space, inodes):
    /// backs `df`. Delegates to `meta::statfs`.
    fn statfs(&self, req: &Request, ino: INodeNo, reply: fuser::ReplyStatfs) {
        let operation = self.for_operation();
        meta::statfs(&operation, req, ino, reply);
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let operation = self.for_operation();
        let result = helpers_for_create::entry(
            &operation,
            req,
            parent.0,
            name,
            CreateEntry::File {
                mode: mode & !umask,
            },
        );
        match result {
            Ok(attr) => {
                let inode = attr.ino.0;
                operation.log_route("create", inode);
                reply.created(
                    &crate::TIMEOUT,
                    &attr,
                    fuser::Generation(0),
                    FileHandle(inode),
                    fuser::FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(e.errno()),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        mode: u32,
        umask: u32,
        reply: fuser::ReplyEntry,
    ) {
        let operation = self.for_operation();
        let result = helpers_for_create::entry(
            &operation,
            req,
            parent.0,
            name,
            CreateEntry::Directory {
                mode: mode & !umask,
            },
        );
        match result {
            Ok(attr) => {
                operation.log_route("mkdir", attr.ino.0);
                reply.entry(&crate::TIMEOUT, &attr, fuser::Generation(0));
            }
            Err(e) => reply.error(e.errno()),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &std::ffi::OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: fuser::ReplyEmpty,
    ) {
        use std::os::unix::ffi::OsStrExt;
        let operation = self.for_operation();
        operation.log_route("setxattr", ino.0);
        let result = if position != 0 {
            Err(crate::store::StoreError::Local(libc::EINVAL))
        } else {
            operation
                .store
                .catalog_inode_in(&operation.context, ino.0)
                .and_then(|entry| {
                    entry
                        .map(|e| e.path)
                        .ok_or(crate::store::StoreError::NotFound)
                })
                .and_then(|path| {
                    operation.store.set_xattr_in(
                        &operation.context,
                        &path,
                        name.as_bytes(),
                        value,
                        flags,
                    )
                })
        };
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(crate::filesystem::ops::error::OpError::from(e).errno()),
        }
    }
    fn getxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &std::ffi::OsStr,
        size: u32,
        reply: fuser::ReplyXattr,
    ) {
        use std::os::unix::ffi::OsStrExt;
        let operation = self.for_operation();
        operation.log_route("getxattr", ino.0);
        let result = operation
            .store
            .catalog_inode_in(&operation.context, ino.0)
            .and_then(|entry| {
                entry
                    .map(|e| e.path)
                    .ok_or(crate::store::StoreError::NotFound)
            })
            .and_then(|path| {
                operation
                    .store
                    .get_xattr_in(&operation.context, &path, name.as_bytes())
            });
        reply_xattr(result, size, reply);
    }
    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: fuser::ReplyXattr) {
        let operation = self.for_operation();
        operation.log_route("listxattr", ino.0);
        let result = operation
            .store
            .catalog_inode_in(&operation.context, ino.0)
            .and_then(|entry| {
                entry
                    .map(|e| e.path)
                    .ok_or(crate::store::StoreError::NotFound)
            })
            .and_then(|path| operation.store.list_xattr_in(&operation.context, &path));
        reply_xattr(result, size, reply);
    }
    fn removexattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &std::ffi::OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        use std::os::unix::ffi::OsStrExt;
        let operation = self.for_operation();
        operation.log_route("removexattr", ino.0);
        let result = operation
            .store
            .catalog_inode_in(&operation.context, ino.0)
            .and_then(|entry| {
                entry
                    .map(|e| e.path)
                    .ok_or(crate::store::StoreError::NotFound)
            })
            .and_then(|path| {
                operation
                    .store
                    .remove_xattr_in(&operation.context, &path, name.as_bytes())
            });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(crate::filesystem::ops::error::OpError::from(e).errno()),
        }
    }

    // misc.rs
    /// Read the target of a symlink (`readlink`). Delegates to
    /// `misc::readlink`.
    fn readlink(&self, req: &Request, ino: INodeNo, reply: fuser::ReplyData) {
        let operation = self.for_operation();
        operation.log_route("readlink", ino.0);
        misc::readlink(&operation, req, ino, reply);
    }

    /// Create a special file (device/fifo/socket) — `mknod`. Delegates
    /// to `misc::mknod`.
    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: fuser::ReplyEntry,
    ) {
        let operation = self.for_operation();
        misc::mknod(&operation, req, parent, name, mode, umask, rdev, reply);
    }

    /// Create a symlink at `parent/link_name` pointing to `target`.
    /// Delegates to `misc::symlink`.
    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &std::ffi::OsStr,
        target: &Path,
        reply: fuser::ReplyEntry,
    ) {
        let operation = self.for_operation();
        misc::symlink(&operation, req, parent, link_name, target, reply);
    }

    /// Create a hard link `newparent/newname` referring to `ino`.
    /// Delegates to `misc::link`.
    fn link(
        &self,
        req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &std::ffi::OsStr,
        reply: fuser::ReplyEntry,
    ) {
        let operation = self.for_operation();
        misc::link(&operation, req, ino, newparent, newname, reply);
    }
}

fn reply_xattr(
    result: Result<Vec<u8>, crate::store::StoreError>,
    size: u32,
    reply: fuser::ReplyXattr,
) {
    match result {
        Ok(data) if size == 0 => reply.size(data.len() as u32),
        Ok(data) if data.len() <= size as usize => reply.data(&data),
        Ok(_) => reply.error(fuser::Errno::ERANGE),
        Err(e) => reply.error(crate::filesystem::ops::error::OpError::from(e).errno()),
    }
}

pub(super) mod helpers_for_create {
    use super::{CreateEntry, Fs, Request};
    use crate::filesystem::ops::{
        error::{OpError, OpResult},
        helpers,
    };
    use std::ffi::OsStr;

    pub(in crate::filesystem) fn entry(
        fs: &Fs,
        req: &Request,
        parent: u64,
        name: &OsStr,
        kind: CreateEntry,
    ) -> OpResult<fuser::FileAttr> {
        create(fs.store.as_ref(), &fs.context, parent, name, kind).and_then(|inode| {
            helpers::attr_or_not_found(fs.store.as_ref(), &fs.context, inode, req.uid(), req.gid())
        })
    }

    pub(in crate::filesystem) fn create(
        store: &dyn crate::StoreApi,
        ctx: &crate::telemetry::OperationContext,
        parent: u64,
        name: &OsStr,
        kind: CreateEntry,
    ) -> OpResult<u64> {
        if name == "." || name == ".." || name.as_encoded_bytes().contains(&b'/') || name.is_empty()
        {
            return Err(OpError::InvalidInput);
        }
        let path = helpers::child_path(&helpers::path_of(store, ctx, parent)?, name)?;
        Ok(store.create_in(ctx, &path, kind)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::FsEntryKind;
    use crate::store::mock::MockStore;

    /// The DI seam works end-to-end: an `Fs` over a fake store serves
    /// attrs without a runtime, a server, or a FUSE session.
    #[test]
    fn attr_for_reads_through_the_injected_store() {
        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        let fs = Fs::with_store(Box::new(store));

        let attr = fs.attr_for(1, 1000, 1000).expect("root is known");
        assert_eq!(attr.ino, INodeNo(1));
        assert_eq!(attr.kind, fuser::FileType::Directory);
        assert_eq!(attr.nlink, 2);
        assert!(fs.attr_for(2, 1000, 1000).is_none());
    }
}
