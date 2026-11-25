//! Filesystem ops that fit none of the other categories: readlink,
//! mknod, symlink, link.
//!
//! The store routes symlink reads to their owner and handles local symlink
//! and special-file creation. Client-only mounts cannot create, and hard
//! links are not implemented.
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

use super::error::{OpError, OpResult};
use super::helpers;
use crate::filesystem::filesystem::Fs;
use crate::filesystem::filesystem::helpers_for_create;
use crate::store::{CreateEntry, StoreApi};

/// Return the target bytes of a symlink from its owning backend.
pub(crate) fn readlink_decision(store: &dyn StoreApi, ino: u64) -> OpResult<Vec<u8>> {
    let path = helpers::path_of(store, ino)?;
    if store.has_conflict(&path) {
        return Err(OpError::Local(libc::EIO));
    }
    Ok(store.readlink(&path)?)
}

fn mknod_decision(
    store: &dyn StoreApi,
    parent: u64,
    name: &OsStr,
    mode: u32,
    rdev: u32,
) -> OpResult<u64> {
    helpers_for_create::create(store, parent, name, CreateEntry::Special { mode, rdev })
}

fn symlink_decision(
    store: &dyn StoreApi,
    parent: u64,
    name: &OsStr,
    target: &Path,
) -> OpResult<u64> {
    helpers_for_create::create(
        store,
        parent,
        name,
        CreateEntry::Symlink {
            target: target.into(),
        },
    )
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
    req: &Request,
    parent: INodeNo,
    name: &OsStr,
    mode: u32,
    umask: u32,
    rdev: u32,
    reply: ReplyEntry,
) {
    match mknod_decision(fs.store.as_ref(), parent.0, name, mode & !umask, rdev).and_then(|inode| {
        helpers::attr_or_not_found(fs.store.as_ref(), inode, req.uid(), req.gid())
    }) {
        Ok(attr) => reply.entry(&crate::TIMEOUT, &attr, fuser::Generation(0)),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn symlink(
    fs: &Fs,
    req: &Request,
    parent: INodeNo,
    link_name: &OsStr,
    target: &Path,
    reply: ReplyEntry,
) {
    match symlink_decision(fs.store.as_ref(), parent.0, link_name, target).and_then(|inode| {
        helpers::attr_or_not_found(fs.store.as_ref(), inode, req.uid(), req.gid())
    }) {
        Ok(attr) => {
            fs.log_route("symlink", attr.ino.0);
            reply.entry(&crate::TIMEOUT, &attr, fuser::Generation(0));
        }
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
    use crate::core::types::FsEntryKind;
    use crate::store::mock::MockStore;

    fn seeded() -> MockStore {
        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        store.insert("/dir", 2, FsEntryKind::Directory);
        store.insert("/dir/f", 3, FsEntryKind::File);
        store
    }

    /// Unsupported creation resolves inodes first, so a missing parent or
    /// target reports ENOENT rather than a misleading policy refusal.
    #[test]
    fn creation_ops_are_refused_after_resolving_inodes() {
        let store = seeded();

        // Each creation-like op resolves its inodes first, then refuses.
        assert_eq!(
            mknod_decision(&store, 2, OsStr::new("dev"), 0, 0).unwrap_err(),
            OpError::NotPermitted
        );
        assert_eq!(
            symlink_decision(&store, 2, OsStr::new("l"), Path::new("/x")).unwrap_err(),
            OpError::NotPermitted
        );
        assert_eq!(
            link_decision(&store, 3, 2, OsStr::new("hard")).unwrap_err(),
            OpError::NotPermitted
        );

        // A nonexistent inode / parent is reported as not-found first.
        assert_eq!(
            readlink_decision(&store, 99).unwrap_err(),
            OpError::NotFound
        );
        assert_eq!(
            mknod_decision(&store, 99, OsStr::new("dev"), 0, 0).unwrap_err(),
            OpError::NotFound
        );
        assert_eq!(
            link_decision(&store, 99, 2, OsStr::new("hard")).unwrap_err(),
            OpError::NotFound
        );
    }
}
