//! The `fs`: our FUSE filesystem. `punkfs` (the whole crate) wires this
//! together with the grpc client, db, etc.
//!
//! Split by operation type. Rust allows at most one `impl Filesystem for Fs`
//! block per type, so each method here is a thin delegate to the free
//! function with the same name in the op modules:
//!
//! - `lookup.rs` — name resolution + metadata: `lookup`, `getattr`
//! - `dir.rs`    — directories: `opendir`, `readdir`
//! - `file.rs`   — file content: `open`, `read`, `write`, `flush`,
//!   `release`, `fsync`
//! - `meta.rs`   — metadata/namespace mutation: `setattr`, `unlink`,
//!   `rmdir`, `rename`, `statfs`
//! - `misc.rs`   — uncategorisable ops: `readlink`, `mknod`, `symlink`,
//!   `link`
//! - `helpers.rs` — helpers that fit none of the above

use crate::filesystem::passthrough::{Local, RoutedStore};
#[cfg(test)]
use crate::filesystem::remote::helpers;
use crate::filesystem::remote::{dir, file, lookup, meta, misc};

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use fuser::FileHandle;
use fuser::Filesystem;
use fuser::INodeNo;
use fuser::MountOption;
use fuser::ReplyAttr;
use fuser::Request;

use crate::store::Store;
use crate::store::StoreApi;
use crate::store::db::metadata_cache::FsNode;

/// The `fs` itself: implements the fuser `Filesystem` trait below.
///
/// All state lives in `store` — the client-side view of the server's
/// directory tree (inodes, paths, which dirs have been listed). See
/// `crate::store`.
///
/// The store is held as a trait object so tests can substitute a fake
/// (see `crate::store::mock`) and exercise the op modules without a
/// server or a FUSE session. Production always gets a real [`Store`].
pub struct Fs {
    /// In-memory snapshot of the server's tree, plus the grpc client used
    /// to fetch listings on demand. `pub(crate)` so the op modules can
    /// reach it without accessors.
    pub(crate) store: Arc<dyn StoreApi>,
    pub(crate) local: Option<Arc<Local>>,
    route_console: bool,
}

impl Fs {
    /// Bind a fresh ingress identity to this FUSE callback and all its store work.
    fn for_operation(&self) -> Self {
        let context = crate::telemetry::OperationContext::new();
        let store: Arc<dyn StoreApi> = if let Some(local) = &self.local {
            Arc::new(RoutedStore {
                inner: self.store.clone(),
                local: local.clone(),
                ctx: context,
            })
        } else {
            Arc::new(crate::store::contextual::ContextualStore {
                inner: self.store.clone(),
                context,
            })
        };
        Self {
            store,
            local: self.local.clone(),
            route_console: self.route_console,
        }
    }

    /// Report the selected cached owner; directories are labeled as merged
    /// because their namespace can have multiple contributors.
    pub(crate) fn log_route(&self, operation: &str, inode: u64) {
        if !self.route_console {
            return;
        }
        let Some(entry) = self.store.get_entry(inode) else {
            return;
        };
        let Some(ctx) = self.store.operation_context() else {
            return;
        };
        let Some(logger) = crate::telemetry::global() else {
            return;
        };
        if !logger.enabled(crate::telemetry::Severity::Info) {
            return;
        }
        let conflict = self.store.has_conflict(&entry.path);
        let directory = entry.kind == crate::store::db::attributes::FsEntryKind::Directory;
        let (owner, location) = if conflict {
            ("unresolved", "conflict")
        } else if directory || entry.node_id.is_empty() {
            ("multiple", "merged")
        } else if self.local.is_some() && entry.node_id == self.store.node_id() {
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
        if let Ok(parent) = crate::filesystem::remote::helpers::path_of(self.store.as_ref(), parent)
            && let Ok(path) = crate::filesystem::remote::helpers::child_path(&parent, name)
            && let Some(inode) = self.store.get_inode(&path)
        {
            self.log_route(operation, inode);
        }
    }

    /// Create an `Fs` bound to a punkfs-server.
    ///
    /// Params:
    /// - `server_url`: grpc endpoint of the server, e.g.
    ///   `"http://127.0.0.1:10000"`. Not contacted yet — the channel is
    ///   lazy, so an unreachable server only fails on the first RPC.
    /// - `node`: this machine's identity (`FsNode`); the store tags
    ///   everything this machine is authoritative for with it.
    ///
    /// Returns: the new `Fs`, or any error from building the tokio
    /// runtime / grpc endpoint. The root inode is seeded by `Store::new`.
    pub fn new(server_url: &str, node: FsNode) -> Result<Self, Box<dyn std::error::Error>> {
        let store = Store::new(server_url, node)?; // root is seeded inside Store::new
        Ok(Fs {
            store: Arc::new(store),
            local: None,
            route_console: false,
        })
    }

    /// Build an `Fs` over an arbitrary store implementation.
    ///
    /// Used by the unit tests (with `crate::store::mock::MockStore`) to
    /// drive the op modules against a catalog with no server behind it.
    #[cfg(test)]
    pub(crate) fn with_store(store: Box<dyn StoreApi>) -> Self {
        Fs {
            store: Arc::from(store),
            local: None,
            route_console: false,
        }
    }

    /// Attributes of `inode`, as served to the kernel.
    #[cfg(test)]
    pub(crate) fn attr_for(&self, inode: u64, uid: u32, gid: u32) -> Option<fuser::FileAttr> {
        helpers::attr_for(self.store.as_ref(), inode, uid, gid)
    }

    /// Mount this fs at `mountpoint` and return immediately: the fs runs
    /// on background threads and the returned `BackgroundSession` keeps
    /// it alive (dropping it unmounts).
    ///
    /// Params:
    /// - `server_url`: passed to `new`.
    /// - `mountpoint`: directory the fs appears under; must exist.
    /// - `node`: this machine's identity, passed to `new`.
    ///
    /// Errors if fuser cannot mount (e.g. missing fusermount3, busy path).
    pub fn mount(
        server_url: &str,
        mountpoint: &Path,
        node: FsNode,
    ) -> Result<fuser::BackgroundSession, Box<dyn std::error::Error>> {
        let fs = Fs::new(server_url, node)?;
        Self::spawn(fs, mountpoint)
    }

    /// Mount several backing nodes as one merged namespace.
    ///
    /// Params:
    /// - `nodes`: `(node_id, url)` pairs. Order is the precedence used when
    ///   two nodes report the same name.
    /// - `mountpoint`: directory the merged view appears under.
    /// - `node`: this machine's identity.
    ///
    /// The nodes are not contacted here: connections are lazy, so a node
    /// that is down at mount time simply contributes nothing until it
    /// answers.
    pub fn mount_cluster(
        nodes: &[(String, String)],
        mountpoint: &Path,
        node: FsNode,
    ) -> Result<fuser::BackgroundSession, Box<dyn std::error::Error>> {
        let store = Store::new_nodes(nodes, node)?;
        Self::spawn(
            Fs {
                store: Arc::new(store),
                local: None,
                route_console: false,
            },
            mountpoint,
        )
    }

    /// Mount a merged view over the existing local directory at `mountpoint`.
    /// The local node must not also appear in `nodes`.
    pub fn mount_with_local(
        nodes: &[(String, String)],
        mountpoint: &Path,
        node: FsNode,
    ) -> Result<fuser::BackgroundSession, Box<dyn std::error::Error>> {
        Self::mount_with_local_store(nodes, mountpoint, node).map(|(session, _)| session)
    }

    /// Mount with opt-in console diagnostics showing paths and backing nodes.
    /// Paths can contain sensitive names; enable this only for trusted use.
    pub fn mount_with_local_console(
        nodes: &[(String, String)],
        mountpoint: &Path,
        node: FsNode,
    ) -> Result<fuser::BackgroundSession, Box<dyn std::error::Error>> {
        Self::mount_with_local_options(nodes, mountpoint, node, true).map(|(session, _)| session)
    }

    /// Mount locally and return the shared catalog for embedders.
    pub fn mount_with_local_store(
        nodes: &[(String, String)],
        mountpoint: &Path,
        node: FsNode,
    ) -> Result<(fuser::BackgroundSession, Arc<Store>), Box<dyn std::error::Error>> {
        Self::mount_with_local_options(nodes, mountpoint, node, false)
    }

    fn mount_with_local_options(
        nodes: &[(String, String)],
        mountpoint: &Path,
        node: FsNode,
        route_console: bool,
    ) -> Result<(fuser::BackgroundSession, Arc<Store>), Box<dyn std::error::Error>> {
        if node.node_id.is_empty()
            || nodes
                .iter()
                .any(|(id, _)| id.is_empty() || id == &node.node_id)
        {
            return Err("node ids must be nonempty and local must not be a remote backend".into());
        }
        let mut seen = std::collections::HashSet::new();
        if nodes.iter().any(|(id, _)| !seen.insert(id)) {
            return Err("remote node ids must be unique".into());
        }
        let local = Local::new(mountpoint, &node.node_id)?;
        Self::mount_with_backing(nodes, mountpoint, node, route_console, local)
    }

    pub(crate) fn mount_with_backing(
        nodes: &[(String, String)],
        mountpoint: &Path,
        node: FsNode,
        route_console: bool,
        local: Arc<Local>,
    ) -> Result<(fuser::BackgroundSession, Arc<Store>), Box<dyn std::error::Error>> {
        let store = Arc::new(Store::new_with_local(nodes, node)?);
        let session = Self::spawn(
            Fs {
                store: store.clone(),
                local: Some(local),
                route_console,
            },
            mountpoint,
        )?;
        Ok((session, store))
    }

    /// Spawn the FUSE session for an already-constructed fs.
    fn spawn(
        fs: Fs,
        mountpoint: &Path,
    ) -> Result<fuser::BackgroundSession, Box<dyn std::error::Error>> {
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
        let Some(local) = &operation.local else {
            reply.error(fuser::Errno::ENOSYS);
            return;
        };
        let result = (|| {
            let parent_path =
                crate::filesystem::remote::helpers::path_of(operation.store.as_ref(), parent.0)?;
            let path = helpers_for_create::path(&operation, parent.0, name)?;
            operation
                .store
                .ensure_listed(&parent_path)
                .map_err(crate::filesystem::remote::error::OpError::from)?;
            let ctx = operation
                .store
                .operation_context()
                .expect("operation context");
            let inode = local
                .create(self.store.as_ref(), ctx, &path, mode & !umask, false)
                .map_err(crate::filesystem::remote::error::OpError::from)?;
            let attr = crate::filesystem::remote::helpers::attr_or_not_found(
                operation.store.as_ref(),
                inode,
                req.uid(),
                req.gid(),
            )?;
            Ok::<_, crate::filesystem::remote::error::OpError>((inode, attr))
        })();
        match result {
            Ok((inode, attr)) => {
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
        let Some(local) = &operation.local else {
            reply.error(fuser::Errno::ENOSYS);
            return;
        };
        let result = (|| {
            let parent_path =
                crate::filesystem::remote::helpers::path_of(operation.store.as_ref(), parent.0)?;
            let path = helpers_for_create::path(&operation, parent.0, name)?;
            operation
                .store
                .ensure_listed(&parent_path)
                .map_err(crate::filesystem::remote::error::OpError::from)?;
            let ctx = operation
                .store
                .operation_context()
                .expect("operation context");
            let inode = local
                .create(self.store.as_ref(), ctx, &path, mode & !umask, true)
                .map_err(crate::filesystem::remote::error::OpError::from)?;
            crate::filesystem::remote::helpers::attr_or_not_found(
                operation.store.as_ref(),
                inode,
                req.uid(),
                req.gid(),
            )
        })();
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
                .get_path(ino.0)
                .ok_or(crate::store::StoreError::NotFound)
                .and_then(|path| {
                    operation
                        .store
                        .set_xattr(&path, name.as_bytes(), value, flags)
                })
        };
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(crate::filesystem::remote::error::OpError::from(e).errno()),
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
            .get_path(ino.0)
            .ok_or(crate::store::StoreError::NotFound)
            .and_then(|path| operation.store.get_xattr(&path, name.as_bytes()));
        reply_xattr(result, size, reply);
    }
    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: fuser::ReplyXattr) {
        let operation = self.for_operation();
        operation.log_route("listxattr", ino.0);
        let result = operation
            .store
            .get_path(ino.0)
            .ok_or(crate::store::StoreError::NotFound)
            .and_then(|path| operation.store.list_xattr(&path));
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
            .get_path(ino.0)
            .ok_or(crate::store::StoreError::NotFound)
            .and_then(|path| operation.store.remove_xattr(&path, name.as_bytes()));
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(crate::filesystem::remote::error::OpError::from(e).errno()),
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
        let Some(local) = &operation.local else {
            misc::mknod(&operation, req, parent, name, mode, umask, rdev, reply);
            return;
        };
        let result = (|| {
            let parent_path =
                crate::filesystem::remote::helpers::path_of(operation.store.as_ref(), parent.0)?;
            let path = helpers_for_create::path(&operation, parent.0, name)?;
            operation
                .store
                .ensure_listed(&parent_path)
                .map_err(crate::filesystem::remote::error::OpError::from)?;
            let ctx = operation
                .store
                .operation_context()
                .expect("operation context");
            let inode = local
                .create_special(self.store.as_ref(), ctx, &path, mode & !umask, rdev)
                .map_err(crate::filesystem::remote::error::OpError::from)?;
            crate::filesystem::remote::helpers::attr_or_not_found(
                operation.store.as_ref(),
                inode,
                req.uid(),
                req.gid(),
            )
        })();
        match result {
            Ok(attr) => reply.entry(&crate::TIMEOUT, &attr, fuser::Generation(0)),
            Err(e) => reply.error(e.errno()),
        }
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
        let Some(local) = &operation.local else {
            misc::symlink(&operation, req, parent, link_name, target, reply);
            return;
        };
        let result = (|| {
            let parent_path =
                crate::filesystem::remote::helpers::path_of(operation.store.as_ref(), parent.0)?;
            let path = helpers_for_create::path(&operation, parent.0, link_name)?;
            operation
                .store
                .ensure_listed(&parent_path)
                .map_err(crate::filesystem::remote::error::OpError::from)?;
            let ctx = operation
                .store
                .operation_context()
                .expect("operation context");
            let inode = local
                .create_symlink(self.store.as_ref(), ctx, &path, target)
                .map_err(crate::filesystem::remote::error::OpError::from)?;
            crate::filesystem::remote::helpers::attr_or_not_found(
                operation.store.as_ref(),
                inode,
                req.uid(),
                req.gid(),
            )
        })();
        match result {
            Ok(attr) => {
                operation.log_route("symlink", attr.ino.0);
                reply.entry(&crate::TIMEOUT, &attr, fuser::Generation(0));
            }
            Err(error) => reply.error(error.errno()),
        }
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
        Err(e) => reply.error(crate::filesystem::remote::error::OpError::from(e).errno()),
    }
}

mod helpers_for_create {
    use super::Fs;
    use crate::filesystem::remote::{
        error::{OpError, OpResult},
        helpers,
    };
    use std::ffi::OsStr;
    pub(super) fn path(fs: &Fs, parent: u64, name: &OsStr) -> OpResult<String> {
        if name == "." || name == ".." || name.as_encoded_bytes().contains(&b'/') || name.is_empty()
        {
            return Err(OpError::InvalidInput);
        }
        helpers::child_path(&helpers::path_of(fs.store.as_ref(), parent)?, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db::attributes::FsEntryKind;
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
