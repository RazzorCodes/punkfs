//! File content I/O: open, read, write, flush, release, fsync.
//!
//! Lifecycle of an open file, as driven by e.g. `cat file`:
//! `lookup` (resolve path) -> `open` -> `read` (maybe repeatedly) ->
//! `flush` (on close) -> `release`. All currently unimplemented:
//! content RPCs don't exist yet in the proto.
use fuser::Errno;
use fuser::FileHandle;
use fuser::FopenFlags;
use fuser::INodeNo;
use fuser::LockOwner;
use fuser::OpenFlags;
use fuser::ReplyData;
use fuser::ReplyEmpty;
use fuser::ReplyOpen;
use fuser::ReplyWrite;
use fuser::Request;
use fuser::WriteFlags;

use super::Fs;

/// Open file `ino`.
///
/// Params:
/// - `_ino`: the file being opened (already resolved via `lookup`).
/// - `_flags`: open flags minus `O_CREAT`/`O_TRUNC` etc.; would carry
///   read/write intent once we act on it.
///
/// Output: `reply.opened` with dummy handle 0 and no `FopenFlags` —
/// stateless, like `opendir`. Read path relies on this succeeding.
/// Delegates nothing; this IS the implementation for now.
pub(crate) fn open(_fs: &Fs, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
    // Stateless open: read/write resolve the inode themselves, so no handle
    // is needed yet.
    reply.opened(FileHandle(0), FopenFlags::empty());
}

/// Read `size` bytes at `offset` from file `ino`.
///
/// Params:
/// - `_offset`: byte position in the file (may be past EOF).
/// - `_size`: max bytes the kernel wants; must be answered with exactly
///   that many unless EOF/error.
/// - `_lock_owner`: for `flock`/POSIX locks; unused.
///
/// Output: ENOSYS — no content RPC exists yet. A real implementation
/// would fetch from the server and `reply.data(&bytes)`.
pub(crate) fn read(
    _fs: &Fs,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _offset: u64,
    _size: u32,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyData,
) {
    reply.error(Errno::ENOSYS);
}

/// Write `data` at `offset` into file `ino`.
///
/// Params:
/// - `_offset`: byte position to write at.
/// - `_data`: bytes to store; must consume all of them on success.
/// - `_write_flags`: page-cache hints (e.g. `FUSE_WRITE_CACHE`).
/// - `_lock_owner`: locking identity; unused.
///
/// Output: ENOSYS. Real implementation: push to server, reply
/// `reply.written(len)` with the full length on success.
pub(crate) fn write(
    _fs: &Fs,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _offset: u64,
    _data: &[u8],
    _write_flags: WriteFlags,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyWrite,
) {
    reply.error(Errno::ENOSYS);
}

/// Called on each `close()` of the file (possibly several per `open`).
///
/// Params:
/// - `_lock_owner`: identifies the locking owner for the close.
///
/// Output: ENOSYS. NOTE: fuser's docs warn errors here can confuse
/// callers (close may already have returned), so a real implementation
/// usually replies ok() unless it must report a write error.
pub(crate) fn flush(
    _fs: &Fs,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _lock_owner: LockOwner,
    reply: ReplyEmpty,
) {
    reply.error(Errno::ENOSYS);
}

/// Tear down the open file — exactly one call per `open`.
///
/// Params:
/// - `_flush`: whether a `flush` should be performed as part of it.
///
/// Output: `reply.ok()`. Errors here are NOT delivered to the
/// application (close/munmap already returned), so success is the only
/// useful answer until state tracking exists.
pub(crate) fn release(
    _fs: &Fs,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    _flush: bool,
    reply: ReplyEmpty,
) {
    // fuser's own default: release succeeds even if unimplemented, since its
    // error is never surfaced to the caller anyway.
    reply.ok();
}

/// Durability barrier (`fsync`/`fdatasync` syscall on the mount).
///
/// Params:
/// - `_datasync`: true = only user data must reach storage, metadata
///   can be skipped (fdatasync semantics).
///
/// Output: ENOSYS. Will matter a lot for the durability contract in the
/// spec (§9): today it is the op that would confirm our writes landed.
pub(crate) fn fsync(
    _fs: &Fs,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _datasync: bool,
    reply: ReplyEmpty,
) {
    reply.error(Errno::ENOSYS);
}
