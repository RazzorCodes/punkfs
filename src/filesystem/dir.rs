//! Directory operations: opendir, readdir.
//!
//! Listing a directory is the other half of the read path: the kernel
//! opens the dir (`opendir`), then repeatedly calls `readdir` with a
//! growing `offset` until we answer with an empty batch.
use std::ffi::OsString;

use fuser::FileHandle;
use fuser::FileType;
use fuser::FopenFlags;
use fuser::INodeNo;
use fuser::OpenFlags;
use fuser::ReplyDirectory;
use fuser::ReplyOpen;
use fuser::Request;

use super::Fs;
use super::error::OpResult;
use super::helpers;
use crate::store::StoreApi;

/// One row of a directory listing, in the shape `reply.add` wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirItem {
    pub inode: u64,
    pub kind: FileType,
    pub name: OsString,
}

/// Everything `readdir` should emit for directory `ino`, already offset.
///
/// Params:
/// - `ino`: inode of the directory being listed.
/// - `offset`: how many entries the kernel already consumed; we skip that
///   many. `.` and `..` are entries 0 and 1, so `offset = 0` re-sends them.
///
/// Behaviour:
/// - Unknown inode is `ENOENT`.
/// - A failed server fetch yields whatever the store has cached — possibly
///   an empty listing — never an error: browsing must not fail because the
///   server is momentarily unreachable.
/// - Children carry their real kind, so `ls -F`/`find` don't see every
///   entry as a regular file.
pub(crate) fn readdir_decision(
    store: &dyn StoreApi,
    ino: u64,
    offset: u64,
) -> OpResult<Vec<DirItem>> {
    let dir_path = helpers::path_of(store, ino)?;

    let mut entries = vec![
        DirItem {
            inode: ino,
            kind: FileType::Directory,
            name: ".".into(),
        },
        DirItem {
            inode: ino,
            kind: FileType::Directory,
            name: "..".into(),
        },
    ];
    entries.extend(store.get_children(&dir_path).into_iter().map(|child| DirItem {
        inode: child.inode,
        kind: helpers::file_type(child.kind),
        name: OsString::from(child.name),
    }));

    Ok(entries.into_iter().skip(offset as usize).collect())
}

/// Start a directory session.
///
/// We keep no per-session state, so the handle is a dummy `0` and
/// `readdir` re-resolves everything from the inode. Never errors;
/// replying ENOSYS here would make `ls` fail outright.
pub(crate) fn opendir(
    _fs: &Fs,
    _req: &Request,
    _ino: INodeNo,
    _flags: OpenFlags,
    reply: ReplyOpen,
) {
    reply.opened(FileHandle(0), FopenFlags::empty());
}

/// Emit one batch of a directory's entries.
///
/// `reply.add` returns `true` once the kernel buffer is full; we stop
/// there and the kernel re-calls us with a higher `offset`.
pub(crate) fn readdir(
    fs: &Fs,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    mut reply: ReplyDirectory,
) {
    let entries = match readdir_decision(fs.store.as_ref(), ino.0, offset) {
        Ok(entries) => entries,
        Err(e) => {
            reply.error(e.errno());
            return;
        }
    };

    for (i, entry) in entries.into_iter().enumerate() {
        let next_offset = offset + i as u64 + 1;
        if reply.add(INodeNo(entry.inode), next_offset, entry.kind, entry.name) {
            break;
        }
    }
    reply.ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::fstree::FsEntryKind;
    use crate::store::mock::MockStore;

    fn seeded() -> MockStore {
        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        store.insert("/docs", 2, FsEntryKind::Directory);
        store.insert("/docs/a.txt", 3, FsEntryKind::File);
        store.insert("/docs/sub", 4, FsEntryKind::Directory);
        store.insert("/docs/link", 5, FsEntryKind::Link);
        store
    }

    fn names(items: &[DirItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| i.name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn readdir_starts_with_dot_and_dotdot_then_children() {
        let store = seeded();
        let items = readdir_decision(&store, 2, 0).unwrap();
        let mut names = names(&items);
        assert_eq!(names.drain(..2).collect::<Vec<_>>(), vec![".", ".."]);
        names.sort();
        assert_eq!(names, vec!["a.txt", "link", "sub"]);
    }

    #[test]
    fn readdir_reports_real_kinds() {
        let store = seeded();
        let items = readdir_decision(&store, 2, 0).unwrap();
        let by_name = items
            .iter()
            .map(|i| (i.name.to_string_lossy().into_owned(), i.kind))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(by_name["a.txt"], FileType::RegularFile);
        assert_eq!(by_name["sub"], FileType::Directory);
        assert_eq!(by_name["link"], FileType::Symlink);
    }

    #[test]
    fn readdir_applies_offset() {
        let store = seeded();
        let all = readdir_decision(&store, 2, 0).unwrap();

        let rest = readdir_decision(&store, 2, 2).unwrap();
        assert_eq!(rest, all[2..].to_vec());
        assert!(names(&rest).iter().all(|n| n != "." && n != ".."));

        // Past the end: empty batch, which tells the kernel we are done.
        assert!(readdir_decision(&store, 2, 99).unwrap().is_empty());
    }

    #[test]
    fn readdir_unknown_inode_is_not_found() {
        let store = seeded();
        assert_eq!(
            readdir_decision(&store, 77, 0).unwrap_err(),
            super::super::error::OpError::NotFound
        );
    }

    #[test]
    fn readdir_serves_cached_children_when_server_is_down() {
        let store = seeded();
        store.fail_get_children();
        let items = readdir_decision(&store, 2, 0).unwrap();
        assert_eq!(names(&items), vec![".", ".."]);
    }

    #[test]
    fn readdir_lists_empty_directories() {
        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        let items = readdir_decision(&store, 1, 0).unwrap();
        assert_eq!(names(&items), vec![".", ".."]);
    }
}
