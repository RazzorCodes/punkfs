//! Helpers shared by the op modules: path joining and the one function
//! that turns a catalog entry into the `FileAttr` the kernel sees.
use std::ffi::OsStr;
use std::path::Path;
use std::time::SystemTime;

use fuser::FileAttr;
use fuser::FileType;
use fuser::INodeNo;

use super::error::{OpError, OpResult};
use crate::db::fstree::FsEntryKind;
use crate::store::StoreApi;

/// Build the attr we serve for `inode`, or `None` if the store has no
/// such entry.
///
/// Fields come from the entry's stored `FsMetadata` when the store knows
/// them, and are synthesized otherwise (serving zeros would tell
/// applications the file is mode 0000 and unusable):
///
/// | field | source | fallback |
/// |---|---|---|
/// | size, blocks, flags | metadata | kept as stored (0 is meaningful) |
/// | kind | `FsEntryKind` | `Unknown` and `File` both present as files |
/// | perm | `metadata.perm` | 0o777 |
/// | uid/gid | `metadata.uid`/`gid` | the *caller's* ids |
/// | nlink | `metadata.nlink` | 2 for dirs (self + `..`), else 1 |
/// | atime/mtime/ctime/crtime | `metadata.*` | now |
/// | blksize/rdev | `metadata.*` | 512 / 0 |
///
/// `uid`/`gid` are the caller's ids, used only as the fallback so an
/// unowned entry still looks usable to whoever is accessing it.
pub(crate) fn attr_for(store: &dyn StoreApi, inode: u64, uid: u32, gid: u32) -> Option<FileAttr> {
    let entry = store.get_entry(inode)?;

    let kind = match entry.kind {
        FsEntryKind::Directory => FileType::Directory,
        FsEntryKind::Link => FileType::Symlink,
        // Unknown (not yet listed) and File both present as files.
        FsEntryKind::Unknown | FsEntryKind::File => FileType::RegularFile,
    };
    let nlink = match entry.metadata.nlink {
        0 if kind == FileType::Directory => 2,
        0 => 1,
        n => n,
    };

    let meta = &entry.metadata;
    let now = SystemTime::now();
    let time = |secs: u64| {
        if secs == 0 {
            now
        } else {
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)
        }
    };

    Some(FileAttr {
        ino: INodeNo(inode),
        size: meta.size,
        blocks: meta.blocks as u64,
        atime: time(meta.atime),
        mtime: time(meta.mtime),
        ctime: time(meta.ctime),
        crtime: time(meta.crtime),
        kind,
        perm: if meta.perm == 0 { 0o777 } else { meta.perm },
        nlink,
        uid: if meta.uid == 0 { uid } else { meta.uid },
        gid: if meta.gid == 0 { gid } else { meta.gid },
        rdev: meta.rdev as u32,
        blksize: if meta.blksize == 0 { 512 } else { meta.blksize as u32 },
        flags: meta.flags,
    })
}

/// Like [`attr_for`], but reports an unknown inode as `ENOENT`.
pub(crate) fn attr_or_not_found(
    store: &dyn StoreApi,
    inode: u64,
    uid: u32,
    gid: u32,
) -> OpResult<FileAttr> {
    attr_for(store, inode, uid, gid).ok_or(OpError::NotFound)
}

/// Resolve a directory inode to its catalog path.
pub(crate) fn path_of(store: &dyn StoreApi, inode: u64) -> OpResult<String> {
    store.get_path(inode).ok_or(OpError::NotFound)
}

/// Join a directory path with a single child name.
///
/// Handles the two names the kernel may pass without them existing in the
/// catalog: `.` resolves to the directory itself and `..` to its parent
/// (`/` is its own parent). Any other name is appended to `parent`.
///
/// Errors with `EINVAL` for a non-UTF8 name — punkfs paths are `String`
/// for now.
pub(crate) fn child_path(parent: &str, name: &OsStr) -> OpResult<String> {
    if name == "." {
        return Ok(parent.to_owned());
    }
    if name == ".." {
        return Ok(parent
            .rsplit_once('/')
            .map(|(head, _)| if head.is_empty() { "/" } else { head })
            .unwrap_or("/")
            .to_owned());
    }

    let joined = Path::new(parent).join(name);
    joined.to_str().map(str::to_owned).ok_or(OpError::InvalidInput)
}
