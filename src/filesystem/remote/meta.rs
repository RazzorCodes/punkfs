//! Metadata and namespace-mutating operations: setattr, unlink, rmdir,
//! rename, statfs.
//!
//! Deliberately absent this iteration:
//!
//! - **Remote creation** — local creation is handled by the passthrough
//!   layer, not the remote backing store.
//! - **Ownership changes** — refused by the backend rather than silently
//!   ignored.
//!
//! `setattr` is *refused* for now: it is not that punkfs will never apply
//! metadata changes, but until write-through metadata semantics are
//! settled, answering with a recoverable `ENOSYS` is better than
//! pretending a truncate or chmod took effect. The backend rpc exists and
//! is tested; only this layer declines to use it.
use std::ffi::OsStr;
use std::time::SystemTime;

use fuser::BsdFileFlags;
use fuser::FileAttr;
use fuser::FileHandle;
use fuser::INodeNo;
use fuser::RenameFlags;
use fuser::ReplyAttr;
use fuser::ReplyEmpty;
use fuser::ReplyStatfs;
use fuser::Request;
use fuser::TimeOrNow;

use super::error::{OpError, OpResult};
use super::helpers;
use crate::core::punkfs::Fs;
use crate::store::db::attributes::FsEntryKind;
use crate::store::{StatFs, StoreApi};

/// Apply the requested attribute changes to `ino`; returns the new attrs.
///
/// `fh` is present for `ftruncate`; it is ignored because content rpcs are
/// positional and the server keys by path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn setattr_decision(
    store: &dyn StoreApi,
    uid: u32,
    gid: u32,
    ino: u64,
    mode: Option<u32>,
    uid_change: Option<u32>,
    gid_change: Option<u32>,
    size: Option<u64>,
    atime: Option<TimeOrNow>,
    mtime: Option<TimeOrNow>,
    ctime: Option<SystemTime>,
    crtime: Option<SystemTime>,
    chgtime: Option<SystemTime>,
    bkuptime: Option<SystemTime>,
    flags: Option<BsdFileFlags>,
) -> OpResult<FileAttr> {
    let _path = helpers::path_of(store, ino)?;

    // Refused wholesale for now, so every field is unused on purpose. A
    // caller sees ENOSYS and knows the change did not happen, which beats a
    // silent no-op that looks like success.
    let _ = (
        mode, uid_change, gid_change, size, atime, mtime, ctime, crtime, chgtime, bkuptime, flags,
        store, uid, gid,
    );
    Err(OpError::Unsupported)
}

/// Remove the file `parent`/`name`.
pub(crate) fn unlink_decision(store: &dyn StoreApi, parent: u64, name: &OsStr) -> OpResult<()> {
    let parent_path = helpers::path_of(store, parent)?;
    let path = helpers::child_path(&parent_path, name)?;
    Ok(store.unlink(&path)?)
}

/// Remove the empty directory `parent`/`name`.
///
/// The emptiness check is a fast local rejection; the backend enforces it
/// authoritatively.
pub(crate) fn rmdir_decision(store: &dyn StoreApi, parent: u64, name: &OsStr) -> OpResult<()> {
    let parent_path = helpers::path_of(store, parent)?;
    let path = helpers::child_path(&parent_path, name)?;

    if store.get_inode(&path).is_some() && !store.get_children(&path).is_empty() {
        return Err(OpError::NotEmpty);
    }
    Ok(store.rmdir(&path)?)
}

/// Move `parent`/`name` to `newparent`/`newname`.
///
/// Directory renames are **unchanged** for now: they keep working as they
/// did before multi-node support. Nesting a directory under a name another
/// node also uses is a known gap, deferred rather than refused.
///
/// The one new restriction, and it applies to files:
///
/// - **Within one node only.** A move that would cross backing nodes is
///   refused with `EXDEV`, the same errno a real filesystem uses when a
///   rename would cross a device. Silently copying the content would be a
///   different operation with different failure modes.
///
/// Flags are passed through: `RENAME_NOREPLACE`/`RENAME_EXCHANGE` are the
/// backend's call to make or refuse.
pub(crate) fn rename_decision(
    store: &dyn StoreApi,
    parent: u64,
    name: &OsStr,
    newparent: u64,
    newname: &OsStr,
    flags: RenameFlags,
) -> OpResult<()> {
    let old_parent = helpers::path_of(store, parent)?;
    let new_parent = helpers::path_of(store, newparent)?;
    let old_path = helpers::child_path(&old_parent, name)?;
    let new_path = helpers::child_path(&new_parent, newname)?;

    // A file keeps the node that holds its bytes; moving it elsewhere would
    // mean copying, which is not a rename. Directories are exempt: their
    // handling is unchanged from before multi-node support.
    let inode = store.get_inode(&old_path).ok_or(OpError::NotFound)?;
    let source = store.get_entry(inode).ok_or(OpError::NotFound)?;

    if source.kind != FsEntryKind::Directory {
        let _ = store.ensure_listed(&new_parent);
        let owner = store.owner_of(&old_path).ok_or(OpError::NotFound)?;
        match store.owner_of(&new_path) {
            // Renaming in place on the same node.
            Some(dest_owner) if dest_owner == owner => {}
            // The destination is new: it is created on the source's node.
            None => {}
            Some(_) => return Err(OpError::CrossDevice),
        }
    }

    Ok(store.rename(&old_path, &new_path, flags)?)
}

/// Aggregate capacity counters for `df`.
pub(crate) fn statfs_decision(store: &dyn StoreApi) -> OpResult<StatFs> {
    Ok(store.statfs()?)
}

pub(crate) fn setattr(
    fs: &Fs,
    req: &Request,
    ino: INodeNo,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    atime: Option<TimeOrNow>,
    mtime: Option<TimeOrNow>,
    ctime: Option<SystemTime>,
    _fh: Option<FileHandle>,
    crtime: Option<SystemTime>,
    chgtime: Option<SystemTime>,
    bkuptime: Option<SystemTime>,
    flags: Option<BsdFileFlags>,
    reply: ReplyAttr,
) {
    let result = setattr_decision(
        fs.store.as_ref(),
        req.uid(),
        req.gid(),
        ino.0,
        mode,
        uid,
        gid,
        size,
        atime,
        mtime,
        ctime,
        crtime,
        chgtime,
        bkuptime,
        flags,
    );
    match result {
        Ok(attr) => reply.attr(&crate::TIMEOUT, &attr),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn unlink(fs: &Fs, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
    match unlink_decision(fs.store.as_ref(), parent.0, name) {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn rmdir(fs: &Fs, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
    match rmdir_decision(fs.store.as_ref(), parent.0, name) {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn rename(
    fs: &Fs,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    newparent: INodeNo,
    newname: &OsStr,
    flags: RenameFlags,
    reply: ReplyEmpty,
) {
    match rename_decision(
        fs.store.as_ref(),
        parent.0,
        name,
        newparent.0,
        newname,
        flags,
    ) {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn statfs(fs: &Fs, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
    match statfs_decision(fs.store.as_ref()) {
        Ok(s) => reply.statfs(
            s.blocks,
            s.bfree,
            s.bavail,
            s.files,
            s.ffree,
            s.block_size,
            s.name_length,
            s.fragment_size,
        ),
        Err(e) => reply.error(e.errno()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StoreError;
    use crate::store::db::attributes::FsEntryKind;
    use crate::store::mock::MockStore;

    fn seeded() -> MockStore {
        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        store.insert("/docs", 2, FsEntryKind::Directory);
        store.insert("/docs/a.txt", 3, FsEntryKind::File);
        store.insert("/docs/empty", 4, FsEntryKind::Directory);
        store
    }

    #[test]
    fn setattr_unknown_inode_is_not_found() {
        let store = seeded();
        let err = setattr_decision(
            &store,
            0,
            0,
            99,
            Some(0o600),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, OpError::NotFound);
    }

    #[test]
    fn every_setattr_is_refused_for_now() {
        // Even a plain chmod: the op layer declines until write-through
        // metadata semantics exist, and says so with a recoverable errno.
        let store = seeded();
        let err = setattr_decision(
            &store,
            5,
            6,
            3,
            Some(0o600),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, OpError::Unsupported);
        assert_eq!(err.errno(), fuser::Errno::ENOSYS);
        assert!(
            store.calls().setattr.is_empty(),
            "a refused request must not reach the backend"
        );
    }

    #[test]
    fn truncation_and_ownership_changes_are_also_refused() {
        let store = seeded();
        for call in [(None, None, Some(10u64)), (None, Some(1000u32), None)] {
            let (mode, uid_change, size) = call;
            let err = setattr_decision(
                &store, 0, 0, 3, mode, uid_change, None, size, None, None, None, None, None, None,
                None,
            )
            .unwrap_err();
            assert_eq!(err, OpError::Unsupported);
        }
        assert!(store.calls().setattr.is_empty());
    }

    #[test]
    fn unlink_joins_the_parent_path() {
        let store = seeded();
        unlink_decision(&store, 2, OsStr::new("a.txt")).unwrap();
        assert_eq!(store.calls().unlink, vec!["/docs/a.txt".to_string()]);
        assert_eq!(store.get_inode("/docs/a.txt"), None);
    }

    #[test]
    fn unlink_unknown_parent_is_not_found() {
        let store = seeded();
        let err = unlink_decision(&store, 99, OsStr::new("x")).unwrap_err();
        assert_eq!(err, OpError::NotFound);
        assert!(store.calls().unlink.is_empty());
    }

    #[test]
    fn rmdir_refuses_non_empty_directory_before_the_store() {
        let store = seeded();
        let err = rmdir_decision(&store, 1, OsStr::new("docs")).unwrap_err();
        assert_eq!(err, OpError::NotEmpty);
        assert!(store.calls().rmdir.is_empty());
    }

    #[test]
    fn rmdir_allows_empty_directory_through() {
        let store = seeded();
        rmdir_decision(&store, 2, OsStr::new("empty")).unwrap();
        assert_eq!(store.calls().rmdir, vec!["/docs/empty".to_string()]);
    }

    #[test]
    fn rename_joins_both_sides() {
        let store = seeded();
        rename_decision(
            &store,
            2,
            OsStr::new("a.txt"),
            1,
            OsStr::new("b.txt"),
            RenameFlags::empty(),
        )
        .unwrap();
        assert_eq!(
            store.calls().rename,
            vec![(
                "/docs/a.txt".to_string(),
                "/b.txt".to_string(),
                RenameFlags::empty()
            )]
        );
        assert!(store.get_inode("/b.txt").is_some());
    }

    #[test]
    fn rename_passes_flags_through() {
        let store = seeded();
        let flags = RenameFlags::RENAME_NOREPLACE;
        rename_decision(&store, 2, OsStr::new("a.txt"), 2, OsStr::new("b"), flags).unwrap();
        assert_eq!(store.calls().rename[0].2, flags);
    }

    #[test]
    fn rename_unknown_source_is_not_found() {
        let store = seeded();
        let err = rename_decision(
            &store,
            2,
            OsStr::new("ghost"),
            1,
            OsStr::new("x"),
            RenameFlags::empty(),
        )
        .unwrap_err();
        assert_eq!(err, OpError::NotFound);
    }

    #[test]
    fn statfs_passes_counters_through() {
        let store = seeded();
        store.expect_statfs(Ok(StatFs {
            blocks: 10,
            block_size: 4096,
            name_length: 255,
            fragment_size: 4096,
            ..StatFs::default()
        }));
        let stats = statfs_decision(&store).unwrap();
        assert_eq!(stats.blocks, 10);
        assert_eq!(stats.block_size, 4096);
    }

    #[test]
    fn statfs_io_error_maps_to_eio() {
        let store = seeded();
        store.expect_statfs(Err(StoreError::Io("down".into())));
        assert_eq!(
            statfs_decision(&store).unwrap_err().errno(),
            fuser::Errno::EIO
        );
    }
}
