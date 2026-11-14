//! Filesystem ops that fit none of the other categories: readlink,
//! mknod, symlink, link.
//!
//! punkfs rejects special files and links outright: EPERM ("operation
//! not permitted" — the caller sees an explicit refusal) rather than
//! ENOSYS, matching fuser's own defaults for symlink/link. Spec §8
//! proposes "resolve symlinks under an explicit security policy" — that
//! policy doesn't exist yet, so refusal is the safe answer.
use std::ffi::OsStr;
use std::path::Path;

use fuser::Errno;
use fuser::INodeNo;
use fuser::ReplyData;
use fuser::ReplyEntry;
use fuser::Request;

use super::Fs;

/// Read symlink target (`readlink`); punkfs stores no symlinks.
/// Params: `ino` — the symlink inode. Output: EPERM.
pub(crate) fn readlink(_fs: &Fs, _req: &Request, _ino: INodeNo, reply: ReplyData) {
    reply.error(Errno::EPERM);
}

/// Create a special file (device node, fifo, socket) via `mknod`.
/// Params: `parent`/`name` location, `mode` type+perms, `umask`,
/// `rdev` device number. Output: EPERM.
pub(crate) fn mknod(
    _fs: &Fs,
    _req: &Request,
    _parent: INodeNo,
    _name: &OsStr,
    _mode: u32,
    _umask: u32,
    _rdev: u32,
    reply: ReplyEntry,
) {
    reply.error(Errno::EPERM);
}

/// Create symlink `parent/link_name` -> `target`.
/// Output: EPERM (no symlink policy yet).
pub(crate) fn symlink(
    _fs: &Fs,
    _req: &Request,
    _parent: INodeNo,
    _link_name: &OsStr,
    _target: &Path,
    reply: ReplyEntry,
) {
    reply.error(Errno::EPERM);
}

/// Create hard link `newparent/newname` -> `ino`.
/// Output: EPERM (spec §8: reject cross-owner hard links rather than
/// silently copying).
pub(crate) fn link(
    _fs: &Fs,
    _req: &Request,
    _ino: INodeNo,
    _newparent: INodeNo,
    _newname: &OsStr,
    reply: ReplyEntry,
) {
    reply.error(Errno::EPERM);
}
