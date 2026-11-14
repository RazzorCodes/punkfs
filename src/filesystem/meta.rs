//! Metadata and namespace-mutating operations: setattr, mkdir, unlink,
//! rmdir, rename, statfs.
//!
//! These change the tree rather than read it, so they all need write-side
//! grpc rpcs before they can do anything: currently every one is a stub.
//! The one-shot generation counter in punkfs's design will be relevant
//! here (each mutation bumps the directory's generation).
use std::ffi::OsStr;
use std::time::SystemTime;

use fuser::BsdFileFlags;
use fuser::Errno;
use fuser::FileHandle;
use fuser::INodeNo;
use fuser::ReplyAttr;
use fuser::ReplyEmpty;
use fuser::ReplyEntry;
use fuser::ReplyStatfs;
use fuser::Request;
use fuser::RenameFlags;
use fuser::TimeOrNow;

use super::Fs;

/// Change attributes of `ino` — the kernel routes chmod, chown, chown
/// root-only size change (truncate), and utimens here.
///
/// Params (each `Some` = requested change, `None` = untouched):
/// - `mode` / `uid` / `gid`: permissions and ownership.
/// - `size`: truncate/extend the file to this length.
/// - `atime` / `mtime` / `ctime`: timestamps (`TimeOrNow` allows the
///   literal meaning of "now").
/// - `fh`: present for `ftruncate` (size change on an open file).
/// - `crtime`/`chgtime`/`bkuptime`, `flags`: macOS/BSD-only fields.
///
/// Output: ENOSYS. Real implementation: sync metadata to the server,
/// then `reply.attr` with the new state.
pub(crate) fn setattr(
    _fs: &Fs,
    _req: &Request,
    _ino: INodeNo,
    _mode: Option<u32>,
    _uid: Option<u32>,
    _gid: Option<u32>,
    _size: Option<u64>,
    _atime: Option<TimeOrNow>,
    _mtime: Option<TimeOrNow>,
    _ctime: Option<SystemTime>,
    _fh: Option<FileHandle>,
    _crtime: Option<SystemTime>,
    _chgtime: Option<SystemTime>,
    _bkuptime: Option<SystemTime>,
    _flags: Option<BsdFileFlags>,
    reply: ReplyAttr,
) {
    reply.error(Errno::ENOSYS);
}

/// Create directory `parent/name`.
///
/// Params:
/// - `mode`: requested permission bits, already masked by the caller's
///   umask on the kernel side too (fuser passes both separately).
/// - `umask`: the process's umask; effective perms = `mode & !umask`.
///
/// Output: ENOSYS. Real implementation: mkdir RPC, then `reply.entry`
/// with the new dir's attrs (like `lookup` returns).
pub(crate) fn mkdir(
    _fs: &Fs,
    _req: &Request,
    _parent: INodeNo,
    _name: &OsStr,
    _mode: u32,
    _umask: u32,
    reply: ReplyEntry,
) {
    reply.error(Errno::ENOSYS);
}

/// Remove file `parent/name` (`rm`).
///
/// Params: parent + name as in `lookup`.
///
/// Output: ENOSYS. Real implementation: unlink RPC; must refuse if the
/// file is still open somewhere or its owner node is offline (spec §9:
/// "reject deletion requiring an offline owner").
pub(crate) fn unlink(_fs: &Fs, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
    reply.error(Errno::ENOSYS);
}

/// Remove directory `parent/name` — only valid if empty (`rmdir`).
///
/// Output: ENOSYS. Real implementation: check no stored children, then
/// rmdir RPC.
pub(crate) fn rmdir(_fs: &Fs, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
    reply.error(Errno::ENOSYS);
}

/// Rename `parent/name` to `newparent/newname` (`mv`).
///
/// Params:
/// - `flags`: `RenameFlags` — may carry `RENAME_NOREPLACE` /
///   `RENAME_EXCHANGE` from `renameat2`.
///
/// Output: ENOSYS. Real implementation gets interesting for punkfs:
/// same-owner rename is atomic on the server; cross-owner is a managed
/// copy+delete per spec §9.
pub(crate) fn rename(
    _fs: &Fs,
    _req: &Request,
    _parent: INodeNo,
    _name: &OsStr,
    _newparent: INodeNo,
    _newname: &OsStr,
    _flags: RenameFlags,
    reply: ReplyEmpty,
) {
    reply.error(Errno::ENOSYS);
}

/// Filesystem-wide stats (`df` on the mount).
///
/// Output: ENOSYS. Real implementation aggregates capacity observations
/// from the catalog — and per spec §5 must report sleeping nodes as
/// last-known, not currently writable.
pub(crate) fn statfs(_fs: &Fs, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
    reply.error(Errno::ENOSYS);
}
