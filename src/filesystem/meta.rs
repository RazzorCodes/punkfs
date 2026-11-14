//! Metadata and namespace-mutating operations: setattr, mkdir, unlink,
//! rmdir, rename, statfs.
//!
//! These change the tree rather than read it. The op layer resolves
//! inodes to catalog paths and hands the request to the store; the store
//! methods are stubs returning `Unimplemented` until the write-side rpcs
//! exist, so these currently surface as `ENOSYS`. Cross-owner semantics
//! (managed copy+delete, refusing deletion via an offline owner) are the
//! store's problem per spec §9, not the op layer's.
use std::ffi::OsStr;
use std::time::SystemTime;

use fuser::BsdFileFlags;
use fuser::FileHandle;
use fuser::FileAttr;
use fuser::INodeNo;
use fuser::ReplyAttr;
use fuser::ReplyEmpty;
use fuser::ReplyEntry;
use fuser::ReplyStatfs;
use fuser::Request;
use fuser::RenameFlags;
use fuser::TimeOrNow;

use super::Fs;
use super::error::{OpError, OpResult};
use super::helpers;
use crate::TIMEOUT;
use crate::store::{SetattrChanges, StatFs, StoreApi};

/// `TimeOrNow` -> the concrete instant it means.
fn resolve_time(time: TimeOrNow, now: SystemTime) -> SystemTime {
    match time {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => now,
    }
}

/// Apply the requested attribute changes to `ino`; returns the new attrs.
///
/// `fh` is present for `ftruncate`; the op layer currently ignores it
/// because the store keys everything by path.
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
    let path = helpers::path_of(store, ino)?;
    let now = SystemTime::now();
    let changes = SetattrChanges {
        mode,
        uid: uid_change,
        gid: gid_change,
        size,
        atime: atime.map(|t| resolve_time(t, now)),
        mtime: mtime.map(|t| resolve_time(t, now)),
        ctime,
        crtime,
        chgtime,
        bkuptime,
        flags: flags.map(|f| f.bits()),
    };

    store.setattr(&path, changes)?;
    helpers::attr_or_not_found(store, ino, uid, gid)
}

/// Create `parent`/`name` and return its inode + attrs (like `lookup`).
pub(crate) fn mkdir_decision(
    store: &dyn StoreApi,
    uid: u32,
    gid: u32,
    parent: u64,
    name: &OsStr,
    mode: u32,
) -> OpResult<(u64, FileAttr)> {
    let parent_path = helpers::path_of(store, parent)?;
    let path = helpers::child_path(&parent_path, name)?;

    let ino = store.mkdir(&path, mode)?;
    let attr = helpers::attr_or_not_found(store, ino, uid, gid)?;
    Ok((ino, attr))
}

/// Remove the file `parent`/`name`.
pub(crate) fn unlink_decision(store: &dyn StoreApi, parent: u64, name: &OsStr) -> OpResult<()> {
    let parent_path = helpers::path_of(store, parent)?;
    let path = helpers::child_path(&parent_path, name)?;
    Ok(store.unlink(&path)?)
}

/// Remove the empty directory `parent`/`name`.
///
/// The emptiness check is a fast local rejection; the store must still
/// enforce it (its view may be newer than ours).
pub(crate) fn rmdir_decision(store: &dyn StoreApi, parent: u64, name: &OsStr) -> OpResult<()> {
    let parent_path = helpers::path_of(store, parent)?;
    let path = helpers::child_path(&parent_path, name)?;

    if store.get_inode(&path).is_some() && !store.get_children(&path).is_empty() {
        return Err(OpError::NotEmpty);
    }
    Ok(store.rmdir(&path)?)
}

/// Move `parent`/`name` to `newparent`/`newname`.
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
        Ok(attr) => reply.attr(&TIMEOUT, &attr),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn mkdir(
    fs: &Fs,
    req: &Request,
    parent: INodeNo,
    name: &OsStr,
    mode: u32,
    umask: u32,
    reply: ReplyEntry,
) {
    let effective = mode & !umask;
    match mkdir_decision(fs.store.as_ref(), req.uid(), req.gid(), parent.0, name, effective) {
        Ok((_ino, attr)) => reply.entry(&TIMEOUT, &attr, fuser::Generation(0)),
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
    match rename_decision(fs.store.as_ref(), parent.0, name, newparent.0, newname, flags) {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn statfs(fs: &Fs, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
    match statfs_decision(fs.store.as_ref()) {
        Ok(s) => reply.statfs(
            s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.frsize,
        ),
        Err(e) => reply.error(e.errno()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::fstree::FsEntryKind;
    use crate::store::StoreError;
    use crate::store::mock::MockStore;

    fn seeded() -> MockStore {
        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        store.insert("/docs", 2, FsEntryKind::Directory);
        store.insert("/docs/a.txt", 3, FsEntryKind::File);
        store
    }

    #[test]
    fn setattr_unknown_inode_is_not_found() {
        let store = seeded();
        let err = setattr_decision(
            &store, 0, 0, 99, Some(0o600), None, None, None, None, None, None, None, None, None, None,
        )
        .unwrap_err();
        assert_eq!(err, OpError::NotFound);
    }

    #[test]
    fn setattr_translates_times_and_forwards_changes() {
        let store = seeded();
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234);
        let err = setattr_decision(
            &store,
            0,
            0,
            3,
            Some(0o640),
            None,
            None,
            Some(10),
            Some(TimeOrNow::SpecificTime(when)),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        // The store stub answers Unimplemented, but the request still made
        // it through with the changes resolved.
        assert_eq!(err, OpError::Unimplemented);
        let changes = store.calls().setattr;
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "/docs/a.txt");
        assert_eq!(changes[0].1.mode, Some(0o640));
        assert_eq!(changes[0].1.size, Some(10));
        assert_eq!(changes[0].1.atime, Some(when));
    }

    #[test]
    fn setattr_returns_new_attrs_on_success() {
        let store = seeded();
        store.expect_setattr(Ok(crate::db::fstree::FsMetadata {
            size: 99,
            perm: 0o600,
            ..Default::default()
        }));
        // The fresh attrs are read back from the store, so seed them too.
        store.insert_with(
            "/docs/a.txt",
            3,
            FsEntryKind::File,
            crate::db::fstree::FsMetadata {
                size: 99,
                perm: 0o600,
                ..Default::default()
            },
        );
        let attr = setattr_decision(
            &store, 5, 6, 3, Some(0o600), None, None, Some(99), None, None, None, None, None, None,
            None,
        )
        .unwrap();
        assert_eq!(attr.size, 99);
        assert_eq!(attr.perm, 0o600);
    }

    #[test]
    fn mkdir_returns_attrs_for_the_new_inode() {
        let store = seeded();
        store.expect_mkdir(Ok(10));
        store.insert("/docs/new", 10, FsEntryKind::Directory);
        let (ino, attr) = mkdir_decision(&store, 5, 6, 2, OsStr::new("new"), 0o755).unwrap();
        assert_eq!(ino, 10);
        assert_eq!(attr.kind, fuser::FileType::Directory);
    }

    #[test]
    fn mkdir_forwards_joined_path() {
        let store = seeded();
        let err = mkdir_decision(&store, 0, 0, 2, OsStr::new("new"), 0o755).unwrap_err();
        assert_eq!(err, OpError::Unimplemented);
        assert_eq!(store.calls().mkdir, vec![("/docs/new".to_string(), 0o755)]);
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
        store.insert("/docs/empty", 4, FsEntryKind::Directory);
        let err = rmdir_decision(&store, 2, OsStr::new("empty")).unwrap_err();
        assert_eq!(err, OpError::Unimplemented); // reached the store stub
        assert_eq!(store.calls().rmdir, vec!["/docs/empty".to_string()]);
    }

    #[test]
    fn rename_joins_both_sides() {
        let store = seeded();
        let err = rename_decision(
            &store,
            2,
            OsStr::new("a.txt"),
            1,
            OsStr::new("b.txt"),
            RenameFlags::empty(),
        )
        .unwrap_err();
        assert_eq!(err, OpError::Unimplemented);
        assert_eq!(
            store.calls().rename,
            vec![(
                "/docs/a.txt".to_string(),
                "/b.txt".to_string(),
                RenameFlags::empty()
            )]
        );
    }

    #[test]
    fn unlink_unknown_parent_is_not_found() {
        let store = seeded();
        let err = unlink_decision(&store, 99, OsStr::new("x")).unwrap_err();
        assert_eq!(err, OpError::NotFound);
        assert!(store.calls().unlink.is_empty());
    }

    #[test]
    fn statfs_passes_counters_through() {
        let store = seeded();
        store.expect_statfs(Ok(StatFs {
            blocks: 10,
            bsize: 4096,
            namelen: 255,
            ..StatFs::default()
        }));
        let stats = statfs_decision(&store).unwrap();
        assert_eq!(stats.blocks, 10);
        assert_eq!(stats.bsize, 4096);
    }

    #[test]
    fn statfs_io_error_maps_to_eio() {
        let store = seeded();
        store.expect_statfs(Err(StoreError::Io("down".into())));
        assert_eq!(statfs_decision(&store).unwrap_err().errno(), fuser::Errno::EIO);
    }
}
