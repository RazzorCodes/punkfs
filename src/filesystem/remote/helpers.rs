//! Helpers shared by the op modules: path joining and the one function
//! that turns a catalog entry into the `FileAttr` the kernel sees.
use std::ffi::OsStr;
use std::path::Path;
use std::time::SystemTime;

use fuser::FileAttr;
use fuser::FileType;
use fuser::INodeNo;

use super::error::{OpError, OpResult};
use crate::store::db::attributes::{FileAttrData, FsEntryKind};
use crate::store::StoreApi;

/// Sentinel for "the server did not tell us" on a timestamp. Entry times
/// are stored as whole seconds since the epoch, so the epoch itself is the
/// only value that can be treated as absent.
fn time_or_now(secs: SystemTime) -> SystemTime {
    if secs == SystemTime::UNIX_EPOCH {
        SystemTime::now()
    } else {
        secs
    }
}

/// Convert catalog kind to the `FileType` the kernel is told.
///
/// `Unknown` presents as a regular file: an entry we have not listed yet
/// still has to answer `stat`, and claiming "unknown" is not an option in
/// the FUSE protocol.
pub(crate) fn file_type(kind: FsEntryKind) -> FileType {
    match kind {
        FsEntryKind::Directory => FileType::Directory,
        FsEntryKind::Link => FileType::Symlink,
        FsEntryKind::Unknown | FsEntryKind::File => FileType::RegularFile,
    }
}

/// Build the attr we serve for an entry.
///
/// Stored values win; anything the catalog has not told us is synthesized
/// so an entry never looks unusable (a mode of 0000 would make files
/// unopenable):
///
/// | field | source | fallback |
/// |---|---|---|
/// | size, blocks, flags | `stat` | kept as stored (0 is meaningful) |
/// | perm | `access.mode` | 0o777 |
/// | uid/gid | `access` | the *caller's* ids |
/// | nlink | `stat.nlink` | 2 for dirs (self + `..`), else 1 |
/// | times | `times` | now |
/// | block_size/rdev | `stat` | 512 / 0 |
pub(crate) fn attr_from(
    inode: u64,
    attr: &FileAttrData,
    uid: u32,
    gid: u32,
) -> FileAttr {
    let kind = file_type(attr.kind);
    let nlink = match attr.stat.nlink {
        0 if kind == FileType::Directory => 2,
        0 => 1,
        n => n,
    };

    FileAttr {
        ino: INodeNo(inode),
        size: attr.stat.size,
        blocks: attr.stat.blocks as u64,
        atime: time_or_now(attr.times.accessed),
        mtime: time_or_now(attr.times.modified),
        ctime: time_or_now(attr.times.changed),
        crtime: time_or_now(attr.times.created),
        kind,
        perm: if attr.access.mode == 0 {
            0o777
        } else {
            attr.access.mode
        },
        nlink,
        uid: if attr.access.uid == 0 {
            uid
        } else {
            attr.access.uid
        },
        gid: if attr.access.gid == 0 {
            gid
        } else {
            attr.access.gid
        },
        rdev: attr.stat.rdev as u32,
        blksize: if attr.stat.block_size == 0 {
            512
        } else {
            attr.stat.block_size as u32
        },
        flags: attr.stat.flags,
    }
}

/// Attributes for `inode`, or `None` if the store has no such entry.
pub(crate) fn attr_for(store: &dyn StoreApi, inode: u64, uid: u32, gid: u32) -> Option<FileAttr> {
    let entry = store.get_entry(inode)?;
    Some(attr_from(inode, &entry.attr, uid, gid))
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

/// Resolve an inode to its catalog path.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db::attributes::{Access, FileTimes, Stat};
    use std::time::{Duration, UNIX_EPOCH};

    fn attr_data(kind: FsEntryKind) -> FileAttrData {
        FileAttrData {
            guid: "u".into(),
            kind,
            ..FileAttrData::default()
        }
    }

    /// Path joining must resolve `.`/`..` arithmetically and reject
    /// non-UTF8 names, so the same helper is safe to feed kernel names.
    #[test]
    fn child_path_joins_resolves_dots_and_rejects_non_utf8() {
        assert_eq!(child_path("/a", OsStr::new("b")).unwrap(), "/a/b");
        assert_eq!(child_path("/", OsStr::new("b")).unwrap(), "/b");
        assert_eq!(child_path("/a/b", OsStr::new(".")).unwrap(), "/a/b");
        assert_eq!(child_path("/a/b", OsStr::new("..")).unwrap(), "/a");
        assert_eq!(child_path("/a", OsStr::new("..")).unwrap(), "/");
        assert_eq!(child_path("/", OsStr::new("..")).unwrap(), "/");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bad = OsStr::from_bytes(&[0xff, 0xfe]);
            assert_eq!(child_path("/a", bad).unwrap_err(), OpError::InvalidInput);
        }
    }

    /// Catalog entries translate into kernel attrs: stored metadata wins,
    /// unknown fields get usable defaults, and every entry kind maps to a
    /// real `FileType`.
    #[test]
    fn attr_from_translates_kinds_and_defaults_or_stored_values() {
        let defaults = attr_from(7, &attr_data(FsEntryKind::File), 1000, 1001);
        assert_eq!(defaults.ino, INodeNo(7));
        assert_eq!(defaults.kind, FileType::RegularFile);
        assert_eq!(defaults.perm, 0o777);
        assert_eq!((defaults.uid, defaults.gid), (1000, 1001));
        assert_eq!(defaults.nlink, 1);
        assert_eq!(defaults.blksize, 512);

        let mut data = attr_data(FsEntryKind::Directory);
        data.access = Access {
            mode: 0o750,
            uid: 33,
            gid: 44,
        };
        data.stat = Stat {
            size: 4096,
            nlink: 5,
            block_size: 4096,
            ..Stat::default()
        };
        data.times.accessed = UNIX_EPOCH + Duration::from_secs(100);

        let stored = attr_from(2, &data, 1000, 1001);
        assert_eq!(stored.kind, FileType::Directory);
        assert_eq!(stored.perm, 0o750);
        assert_eq!((stored.uid, stored.gid), (33, 44));
        assert_eq!(stored.nlink, 5);
        assert_eq!(stored.size, 4096);
        assert_eq!(stored.blksize, 4096);
        assert_eq!(stored.atime, UNIX_EPOCH + Duration::from_secs(100));

        assert_eq!(file_type(FsEntryKind::Link), FileType::Symlink);
        assert_eq!(file_type(FsEntryKind::Unknown), FileType::RegularFile);
        assert_eq!(file_type(FsEntryKind::File), FileType::RegularFile);
    }

    /// A directory defaults to two links and epoch times become "now", so
    /// nothing the server left unset shows up as 1970.
    #[test]
    fn defaults_for_dirs_and_epoch_times() {
        assert_eq!(
            attr_from(1, &attr_data(FsEntryKind::Directory), 0, 0).nlink,
            2
        );
        let attr = attr_from(1, &attr_data(FsEntryKind::File), 0, 0);
        assert!(attr.mtime > UNIX_EPOCH + Duration::from_secs(1_000_000));

        let mut data = attr_data(FsEntryKind::File);
        data.times = FileTimes::from_secs(1, 2, 3, 4);
        let carried = attr_from(1, &data, 0, 0);
        assert_eq!(carried.atime, UNIX_EPOCH + Duration::from_secs(1));
        assert_eq!(carried.mtime, UNIX_EPOCH + Duration::from_secs(2));
        assert_eq!(carried.ctime, UNIX_EPOCH + Duration::from_secs(3));
        assert_eq!(carried.crtime, UNIX_EPOCH + Duration::from_secs(4));
    }
}
