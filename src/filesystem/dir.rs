//! Directory operations: opendir, readdir.
//!
//! Listing a directory is the other half of the read path: the kernel
//! opens the dir (`opendir`), then repeatedly calls `readdir` with a
//! growing `offset` until we answer with an empty batch.
use std::ffi::OsString;

use fuser::Errno;
use fuser::FileHandle;
use fuser::FileType;
use fuser::FopenFlags;
use fuser::INodeNo;
use fuser::OpenFlags;
use fuser::ReplyDirectory;
use fuser::ReplyOpen;
use fuser::Request;

use super::Fs;

/// Start a directory session.
///
/// Params:
/// - `_fs`/`_req`: unused — we keep no per-session state.
/// - `_ino`: the directory being opened.
/// - `_flags`: open flags (read/write intent); ignored.
///
/// Output: `reply.opened` with a dummy handle `0` (stateless — `readdir`
/// re-resolves everything from the inode, so there is nothing to store).
/// Never errors; replying ENOSYS here would make `ls` fail outright.
pub(crate) fn opendir(_fs: &Fs, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
    // Stateless: we don't need a handle, readdir re-fetches by inode.
    reply.opened(FileHandle(0), FopenFlags::empty());
}

/// Emit one batch of a directory's entries.
///
/// Params:
/// - `fs`: to reach the store (path of `ino`, stored children).
/// - `ino`: inode of the directory being listed.
/// - `_fh`: handle from `opendir`; unused (stateless).
/// - `offset`: how many entries the kernel already consumed; we skip
///   that many. `.` and `..` are entries 0 and 1.
/// - `reply`: accumulate entries with `reply.add`; the first `Ok(true)`
///   return means the kernel buffer is full — stop and let it re-call.
///
/// Output: `reply.ok()` when all remaining entries were added. On an
/// unknown inode: ENOENT. Children come from the store; a failed server
/// fetch yields whatever is stored (possibly an empty listing), never
/// an error — browsing offline is a spec requirement.
pub(crate) fn readdir(
    fs: &Fs,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    mut reply: ReplyDirectory,
) {
    let Some(dir_path) = fs.store.get_path(ino.0) else {
        reply.error(Errno::ENOENT);
        return;
    };
    let mut entries: Vec<(u64, FileType, OsString)> = vec![
        (ino.0, FileType::Directory, ".".into()),
        (ino.0, FileType::Directory, "..".into()),
    ];
    entries.extend(
        fs.store
            .get_children(&dir_path)
            .unwrap_or_default()
            .into_iter()
            .map(|(inode, name)| (inode, FileType::RegularFile, OsString::from(name))),
    );
    for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
        if reply.add(INodeNo(child_ino), (i + 1) as u64, kind, name) {
            break;
        }
    }
    reply.ok();
}
