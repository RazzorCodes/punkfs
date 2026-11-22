//! Filesystem ops that fit none of the other categories: readlink,
//! mknod, symlink, link.
//!
//! punkfs rejects special files and links outright: `EPERM` ("operation
//! not permitted" — the caller sees an explicit refusal) rather than
//! `ENOSYS`, matching fuser's own defaults for symlink/link. Creating any
//! new entry through the merged mount is out of scope for now, and
//! symlinks additionally need a resolution policy that does not exist yet.
//!
//! Each decision still resolves its inodes through the store first, so an
//! op on a nonexistent parent reports `ENOENT` rather than a misleading
//! policy error.
use std::ffi::OsStr;
use std::path::Path;

use fuser::INodeNo;
use fuser::ReplyData;
use fuser::ReplyEntry;
use fuser::Request;

use super::Fs;
use super::error::{OpError, OpResult};
use super::helpers;
use crate::store::StoreApi;

/// Read a symlink target; punkfs stores no symlinks, so always `EPERM`
/// after confirming the inode exists.
pub(crate) fn readlink_decision(store: &dyn StoreApi, ino: u64) -> OpResult<Vec<u8>> {
    let _ = helpers::path_of(store, ino)?;
    Err(OpError::NotPermitted)
}

/// Create a device node / fifo / socket: always refused.
pub(crate) fn mknod_decision(
    store: &dyn StoreApi,
    parent: u64,
    name: &OsStr,
) -> OpResult<(u64, fuser::FileAttr)> {
    let parent_path = helpers::path_of(store, parent)?;
    let _ = helpers::child_path(&parent_path, name)?;
    Err(OpError::NotPermitted)
}

/// Create a symlink: always refused (no symlink policy yet).
pub(crate) fn symlink_decision(
    store: &dyn StoreApi,
    parent: u64,
    link_name: &OsStr,
    _target: &Path,
) -> OpResult<(u64, fuser::FileAttr)> {
    let parent_path = helpers::path_of(store, parent)?;
    let _ = helpers::child_path(&parent_path, link_name)?;
    Err(OpError::NotPermitted)
}

/// Create a hard link: always refused (spec §8: reject cross-owner hard
/// links rather than silently copying).
pub(crate) fn link_decision(
    store: &dyn StoreApi,
    ino: u64,
    newparent: u64,
    newname: &OsStr,
) -> OpResult<(u64, fuser::FileAttr)> {
    let _ = helpers::path_of(store, ino)?;
    let parent_path = helpers::path_of(store, newparent)?;
    let _ = helpers::child_path(&parent_path, newname)?;
    Err(OpError::NotPermitted)
}

pub(crate) fn readlink(fs: &Fs, _req: &Request, ino: INodeNo, reply: ReplyData) {
    match readlink_decision(fs.store.as_ref(), ino.0) {
        Ok(target) => reply.data(&target),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn mknod(
    fs: &Fs,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    _mode: u32,
    _umask: u32,
    _rdev: u32,
    reply: ReplyEntry,
) {
    match mknod_decision(fs.store.as_ref(), parent.0, name) {
        Ok((_, attr)) => reply.entry(&crate::TIMEOUT, &attr, fuser::Generation(0)),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn symlink(
    fs: &Fs,
    _req: &Request,
    parent: INodeNo,
    link_name: &OsStr,
    target: &Path,
    reply: ReplyEntry,
) {
    match symlink_decision(fs.store.as_ref(), parent.0, link_name, target) {
        Ok((_, attr)) => reply.entry(&crate::TIMEOUT, &attr, fuser::Generation(0)),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn link(
    fs: &Fs,
    _req: &Request,
    ino: INodeNo,
    newparent: INodeNo,
    newname: &OsStr,
    reply: ReplyEntry,
) {
    match link_decision(fs.store.as_ref(), ino.0, newparent.0, newname) {
        Ok((_, attr)) => reply.entry(&crate::TIMEOUT, &attr, fuser::Generation(0)),
        Err(e) => reply.error(e.errno()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db::attributes::FsEntryKind;
    use crate::store::mock::MockStore;

    fn seeded() -> MockStore {
        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        store.insert("/dir", 2, FsEntryKind::Directory);
        store.insert("/dir/f", 3, FsEntryKind::File);
        store
    }

    #[test]
    fn readlink_existing_inode_is_refused() {
        let store = seeded();
        assert_eq!(readlink_decision(&store, 3).unwrap_err(), OpError::NotPermitted);
    }

    #[test]
    fn readlink_unknown_inode_is_not_found() {
        let store = seeded();
        assert_eq!(readlink_decision(&store, 99).unwrap_err(), OpError::NotFound);
    }

    #[test]
    fn mknod_refuses_after_resolving_parent() {
        let store = seeded();
        assert_eq!(
            mknod_decision(&store, 2, OsStr::new("dev")).unwrap_err(),
            OpError::NotPermitted
        );
        // A missing parent is reported as such, not as a policy refusal.
        assert_eq!(
            mknod_decision(&store, 99, OsStr::new("dev")).unwrap_err(),
            OpError::NotFound
        );
    }

    #[test]
    fn symlink_and_link_are_refused() {
        let store = seeded();
        assert_eq!(
            symlink_decision(&store, 2, OsStr::new("l"), Path::new("/x")).unwrap_err(),
            OpError::NotPermitted
        );
        assert_eq!(
            link_decision(&store, 3, 2, OsStr::new("hard")).unwrap_err(),
            OpError::NotPermitted
        );
    }

    #[test]
    fn link_unknown_source_is_not_found() {
        let store = seeded();
        assert_eq!(
            link_decision(&store, 99, 2, OsStr::new("hard")).unwrap_err(),
            OpError::NotFound
        );
    }
}
