//! Path resolution and metadata queries: lookup, getattr.
//!
//! These are the two ops every other filesystem activity starts with:
//! the kernel asks `lookup` to turn a (parent dir, name) pair into an
//! inode, and `getattr` to describe any inode.
//!
//! Each op is split in two:
//! - `*_decision` does the work against `&dyn StoreApi` and plain values,
//!   so it is unit testable (see the `tests` module at the bottom).
//! - the same-named FUSE function unwraps `Request`/`Reply` and maps the
//!   decision result onto the reply.
use std::ffi::OsStr;

use fuser::FileAttr;
use fuser::FileHandle;
use fuser::Generation;
use fuser::INodeNo;
use fuser::ReplyAttr;
use fuser::ReplyEntry;
use fuser::Request;

use super::error::{OpError, OpResult};
use super::helpers;
use crate::TIMEOUT;
use crate::filesystem::filesystem::Fs;
use crate::store::StoreApi;

/// Resolve `parent`/`name` to an inode and its attributes.
///
/// Params:
/// - `store`: the catalog to resolve against.
/// - `uid`/`gid`: the caller's ids (fallback owners in `attr_for`).
/// - `parent`: inode of the containing directory.
/// - `name`: child entry name; `.` and `..` are handled without a lookup.
///
/// Behaviour:
/// - An unknown `parent` is `ENOENT`.
/// - For a normal name the parent's listing is refreshed from the server
///   first; a failed fetch is swallowed so lookups keep working from the
///   cached listing.
/// - A name not present in the listing is `ENOENT`.
/// - A non-UTF8 name is `EINVAL`.
pub(crate) fn lookup_decision(
    store: &dyn StoreApi,
    uid: u32,
    gid: u32,
    parent: u64,
    name: &OsStr,
) -> OpResult<(u64, FileAttr)> {
    let parent_path = helpers::path_of(store, parent)?;
    let full_path = helpers::child_path(&parent_path, name)?;

    // `.`/`..` are resolved arithmetically; only real children need a
    // server round trip.
    if name != "." && name != ".." {
        let _ = store.ensure_listed(&parent_path);
    }

    if store.has_conflict(&full_path) {
        return Err(OpError::Local(libc::EIO));
    }
    let entry = store.catalog_path(&full_path)?.ok_or(OpError::NotFound)?;
    // A publication can populate the name before its first write reaches
    // the owner. Return the owner's current size for a remote regular file:
    // the kernel may use lookup attributes to calculate an O_APPEND offset.
    let attr = if store.has_local_backing()
        && entry.kind == crate::core::types::FsEntryKind::File
        && entry.node_id != store.node_id()
    {
        store.refresh(&entry.path).unwrap_or(entry.attr)
    } else {
        entry.attr
    };
    Ok((
        entry.inode,
        helpers::attr_from(entry.inode, &attr, uid, gid),
    ))
}

/// Attributes of an inode.
///
/// The server is asked first so `stat` reflects reality (a remote write
/// changes size/mtime); if that fails we fall back to the cached entry,
/// which is what keeps an offline directory usable.
pub(crate) fn getattr_decision(
    store: &dyn StoreApi,
    uid: u32,
    gid: u32,
    ino: u64,
) -> OpResult<FileAttr> {
    let entry = store.catalog_inode(ino)?.ok_or(OpError::NotFound)?;
    if store.has_conflict(&entry.path) {
        return Err(OpError::Local(libc::EIO));
    }
    let attr = store.refresh(&entry.path).unwrap_or(entry.attr);
    Ok(helpers::attr_from(ino, &attr, uid, gid))
}

pub(crate) fn lookup(fs: &Fs, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
    match lookup_decision(fs.store.as_ref(), req.uid(), req.gid(), parent.0, name) {
        // Zero TTL so the kernel re-asks us instead of caching.
        Ok((inode, attr)) => {
            fs.log_route("lookup", inode);
            reply.entry(&TIMEOUT, &attr, Generation(0));
        }
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn getattr(
    fs: &Fs,
    req: &Request,
    ino: INodeNo,
    _fh: Option<FileHandle>,
    reply: ReplyAttr,
) {
    match getattr_decision(fs.store.as_ref(), req.uid(), req.gid(), ino.0) {
        Ok(attr) => {
            fs.log_route("getattr", ino.0);
            reply.attr(&TIMEOUT, &attr);
        }
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
        store.insert("/docs", 2, FsEntryKind::Directory);
        store.insert("/docs/readme", 3, FsEntryKind::File);
        store
    }

    /// Name resolution walks parent path -> listing -> inode and back to
    /// attrs, and `.`/`..` resolve arithmetically without a server round
    /// trip.
    #[test]
    fn lookup_resolves_and_dot_names_skip_the_network() {
        let store = seeded();
        let (ino, _) = lookup_decision(&store, 7, 8, 2, OsStr::new("readme")).unwrap();
        assert_eq!(ino, 3);
        assert_eq!(store.listed(), vec!["/docs".to_string()]);

        assert_eq!(
            lookup_decision(&store, 7, 8, 2, OsStr::new(".")).unwrap().0,
            2
        );
        assert_eq!(
            lookup_decision(&store, 7, 8, 2, OsStr::new(".."))
                .unwrap()
                .0,
            1
        );
        assert!(
            store.listed().iter().filter(|d| **d == "/docs").count() == 1,
            "dot names must not trigger another listing"
        );
    }

    /// Browse must survive a momentary outage: a failed listing is swallowed
    /// and the cached answer is served.
    #[test]
    fn lookup_offline_failure_is_swallowed() {
        let store = seeded();
        store.fail_ensure_listed(crate::store::StoreError::Io("down".into()));
        let (ino, _) = lookup_decision(&store, 7, 8, 2, OsStr::new("readme")).unwrap();
        assert_eq!(ino, 3);
    }

    /// Unknown names and parents report ENOENT; a refresh that fails still
    /// serves the cached metadata.
    #[test]
    fn missing_or_unreadable_entries_report_not_found() {
        let store = seeded();
        assert_eq!(
            lookup_decision(&store, 7, 8, 2, OsStr::new("nope")).unwrap_err(),
            OpError::NotFound
        );
        assert_eq!(
            lookup_decision(&store, 7, 8, 99, OsStr::new("readme")).unwrap_err(),
            OpError::NotFound
        );
        assert_eq!(
            getattr_decision(&store, 7, 8, 42).unwrap_err(),
            OpError::NotFound
        );
    }

    /// getattr keeps answering from the cache when refresh fails, but serves
    /// freshly refreshed metadata (size/mode/owner) when it succeeds.
    #[test]
    fn getattr_refreshes_or_falls_back_to_the_cache() {
        use crate::core::types::{Access, FileAttrData, Stat};

        let offline = seeded();
        offline.fail_refresh();
        assert_eq!(getattr_decision(&offline, 7, 8, 3).unwrap().ino, INodeNo(3));

        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        store.insert_with(
            "/f",
            5,
            FsEntryKind::File,
            FileAttrData {
                guid: "guid-f".into(),
                kind: FsEntryKind::File,
                access: Access {
                    mode: 0o640,
                    uid: 33,
                    gid: 44,
                },
                stat: Stat {
                    size: 1234,
                    ..Stat::default()
                },
                ..FileAttrData::default()
            },
        );
        let attr = getattr_decision(&store, 7, 8, 5).unwrap();
        assert_eq!(attr.size, 1234);
        assert_eq!(attr.perm, 0o640);
        assert_eq!((attr.uid, attr.gid), (33, 44));
    }
}
