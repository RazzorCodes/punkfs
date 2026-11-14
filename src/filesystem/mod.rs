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
//! - `meta.rs`   — metadata/namespace mutation: `setattr`, `mkdir`,
//!   `unlink`, `rmdir`, `rename`, `statfs`
//! - `misc.rs`   — uncategorisable ops: `readlink`, `mknod`, `symlink`,
//!   `link`
//! - `helpers.rs` — helpers that fit none of the above

mod dir;
mod file;
mod helpers;
mod lookup;
mod meta;
mod misc;

use std::path::Path;
use std::time::SystemTime;

use fuser::FileHandle;
use fuser::FileAttr;
use fuser::Filesystem;
use fuser::FileType;
use fuser::INodeNo;
use fuser::MountOption;
use fuser::ReplyAttr;
use fuser::Request;

use crate::cache::Cache;


/// The `fs` itself: implements the fuser `Filesystem` trait below.
///
/// All state lives in `cache` — the client-side view of the server's
/// directory tree (inodes, paths, which dirs have been listed). See
/// `crate::cache`.
pub struct Fs {
    /// In-memory snapshot of the server's tree, plus the grpc client used
    /// to fetch listings on demand. `pub(crate)` so the op modules can
    /// reach it without accessors.
    pub(crate) cache: Cache,
}

impl Fs {
    /// Create an `Fs` bound to a punkfs-server.
    ///
    /// Params:
    /// - `server_url`: grpc endpoint of the server, e.g.
    ///   `"http://127.0.0.1:10000"`. Not contacted yet — the channel is
    ///   lazy, so an unreachable server only fails on the first RPC.
    ///
    /// Returns: the new `Fs`, or any error from building the tokio
    /// runtime / grpc endpoint. The root inode is seeded by `Cache::new`.
    pub fn new(server_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let cache = Cache::new(server_url)?; // root is seeded inside Cache::new
        Ok(Fs { cache })
    }

    /// Mount this fs at `mountpoint` and return immediately: the fs runs
    /// on background threads and the returned `BackgroundSession` keeps
    /// it alive (dropping it unmounts).
    ///
    /// Params:
    /// - `server_url`: passed to `new`.
    /// - `mountpoint`: directory the fs appears under; must exist.
    ///
    /// Errors if fuser cannot mount (e.g. missing fusermount3, busy path).
    pub fn mount(
        server_url: &str,
        mountpoint: &Path,
    ) -> Result<fuser::BackgroundSession, Box<dyn std::error::Error>> {
        let fs = Fs::new(server_url)?;

        let mut cfg = fuser::Config::default();
        cfg.mount_options = vec![MountOption::FSName("punkfs".into())];

        Ok(fuser::spawn_mount(fs, mountpoint, &cfg)?)
    }

    /// Build the attr we serve for `inode`. Every op that must answer with
    /// metadata goes through here so the answer stays consistent.
    ///
    /// All metadata is currently synthesized: size 0, times = now,
    /// perm 0777. Only `/` is a directory; every other path is reported
    /// as a regular file — a known simplification.
    ///
    /// Params:
    /// - `inode`: which entry to describe (resolved via `cache`).
    /// - `uid`/`gid`: the *caller's* ids, stamped into the attr so the
    ///   kernel sees the file as owned by whoever is accessing it.
    ///
    /// Returns `Some(FileAttr)` if the inode is known, else `None`
    /// (callers turn that into `ENOENT`).
    pub(crate) fn attr_for(&self, inode: u64, uid: u32, gid: u32) -> Option<FileAttr> {
        let path = self.cache.get_path(inode)?;

        let (kind, nlink) = if path == "/" {
            (FileType::Directory, 2)
        } else {
            (FileType::RegularFile, 1)
        };

        let now = SystemTime::now();
        Some(FileAttr {
            ino: INodeNo(inode),
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind,
            perm: 0o777,
            nlink,
            uid,
            gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        })
    }
}

/// The single `impl Filesystem for Fs`. Every method delegates to the free
/// function of the same name in its op module.
impl Filesystem for Fs {
    // lookup.rs
    /// Resolve `name` (a child of directory `parent`) to an inode and its
    /// attrs. Triggered by every stat-like call and as the first half of
    /// any path-based access. Delegates to `lookup::lookup`.
    fn lookup(&self, req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: fuser::ReplyEntry) {
        lookup::lookup(self, req, parent, name, reply);
    }

    /// Answer with the attributes of `ino` (stat). Delegates to
    /// `lookup::getattr`.
    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        lookup::getattr(self, req, ino, fh, reply);
    }

    // dir.rs
    /// Begin a directory read session; `fh` returned is passed to later
    /// `readdir` calls. Delegates to `dir::opendir`.
    fn opendir(&self, req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: fuser::ReplyOpen) {
        dir::opendir(self, req, ino, flags, reply);
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
        dir::readdir(self, req, ino, fh, offset, reply);
    }

    // file.rs
    /// Open a file; the returned `fh` is passed to `read`/`write`/etc.
    /// Delegates to `file::open`.
    fn open(&self, req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: fuser::ReplyOpen) {
        file::open(self, req, ino, flags, reply);
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
        file::read(self, req, ino, fh, offset, size, flags, lock_owner, reply);
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
        file::write(self, req, ino, fh, offset, data, write_flags, flags, lock_owner, reply);
    }

    /// Called on every `close()` of an open file (may be several times
    /// per `open` due to `dup`/fork). Delegates to `file::flush`.
    fn flush(&self, req: &Request, ino: INodeNo, fh: FileHandle, lock_owner: fuser::LockOwner, reply: fuser::ReplyEmpty) {
        file::flush(self, req, ino, fh, lock_owner, reply);
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
        file::release(self, req, ino, fh, flags, lock_owner, flush, reply);
    }

    /// Flush file contents to stable storage (`fsync`/`fdatasync`):
    /// `datasync` true means skip metadata. Delegates to `file::fsync`.
    fn fsync(&self, req: &Request, ino: INodeNo, fh: FileHandle, datasync: bool, reply: fuser::ReplyEmpty) {
        file::fsync(self, req, ino, fh, datasync, reply);
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
        meta::setattr(
            self, req, ino, mode, uid, gid, size, atime, mtime, ctime, fh, crtime, chgtime,
            bkuptime, flags, reply,
        );
    }

    /// Create a directory named `name` inside `parent` with permission
    /// `mode & !umask`. Delegates to `meta::mkdir`.
    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        mode: u32,
        umask: u32,
        reply: fuser::ReplyEntry,
    ) {
        meta::mkdir(self, req, parent, name, mode, umask, reply);
    }

    /// Remove the file `name` inside `parent`. Delegates to
    /// `meta::unlink`.
    fn unlink(&self, req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: fuser::ReplyEmpty) {
        meta::unlink(self, req, parent, name, reply);
    }

    /// Remove the empty directory `name` inside `parent`. Delegates to
    /// `meta::rmdir`.
    fn rmdir(&self, req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: fuser::ReplyEmpty) {
        meta::rmdir(self, req, parent, name, reply);
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
        meta::rename(self, req, parent, name, newparent, newname, flags, reply);
    }

    /// Report the filesystem's overall stats (free space, inodes):
    /// backs `df`. Delegates to `meta::statfs`.
    fn statfs(&self, req: &Request, ino: INodeNo, reply: fuser::ReplyStatfs) {
        meta::statfs(self, req, ino, reply);
    }

    // misc.rs
    /// Read the target of a symlink (`readlink`). Delegates to
    /// `misc::readlink`.
    fn readlink(&self, req: &Request, ino: INodeNo, reply: fuser::ReplyData) {
        misc::readlink(self, req, ino, reply);
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
        misc::mknod(self, req, parent, name, mode, umask, rdev, reply);
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
        misc::symlink(self, req, parent, link_name, target, reply);
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
        misc::link(self, req, ino, newparent, newname, reply);
    }
}
