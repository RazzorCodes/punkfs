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

use super::Fs;
use super::error::{OpError, OpResult};
use super::helpers;
use crate::TIMEOUT;
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

    let inode = store.get_inode(&full_path).ok_or(OpError::NotFound)?;
    let attr = helpers::attr_or_not_found(store, inode, uid, gid)?;
    Ok((inode, attr))
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
    if let Some(path) = store.get_path(ino)
        && store.refresh(&path).is_ok()
    {
        // The refresh stored the fresh attributes; serve from there.
        return helpers::attr_or_not_found(store, ino, uid, gid);
    }

    helpers::attr_or_not_found(store, ino, uid, gid)
}

pub(crate) fn lookup(fs: &Fs, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
    match lookup_decision(fs.store.as_ref(), req.uid(), req.gid(), parent.0, name) {
        // Zero TTL so the kernel re-asks us instead of caching.
        Ok((_inode, attr)) => reply.entry(&TIMEOUT, &attr, Generation(0)),
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
        Ok(attr) => reply.attr(&TIMEOUT, &attr),
        Err(e) => reply.error(e.errno()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db::attributes::FsEntryKind;
    use crate::store::StoreError;
    use crate::store::mock::MockStore;

    fn seeded() -> MockStore {
        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        store.insert("/docs", 2, FsEntryKind::Directory);
        store.insert("/docs/readme", 3, FsEntryKind::File);
        store
    }

    #[test]
    fn lookup_resolves_child_and_lists_parent() {
        let store = seeded();
        let (ino, attr) = lookup_decision(&store, 7, 8, 2, OsStr::new("readme")).unwrap();
        assert_eq!(ino, 3);
        assert_eq!(attr.ino, INodeNo(3));
        assert_eq!(store.listed(), vec!["/docs".to_string()]);
    }

    #[test]
    fn lookup_unknown_name_is_not_found() {
        let store = seeded();
        let err = lookup_decision(&store, 7, 8, 2, OsStr::new("nope")).unwrap_err();
        assert_eq!(err, OpError::NotFound);
    }

    #[test]
    fn lookup_unknown_parent_is_not_found() {
        let store = seeded();
        let err = lookup_decision(&store, 7, 8, 99, OsStr::new("readme")).unwrap_err();
        assert_eq!(err, OpError::NotFound);
    }

    #[test]
    fn lookup_dot_and_dotdot_self_resolve() {
        let store = seeded();
        let (ino, _) = lookup_decision(&store, 7, 8, 2, OsStr::new(".")).unwrap();
        assert_eq!(ino, 2);
        let (ino, _) = lookup_decision(&store, 7, 8, 2, OsStr::new("..")).unwrap();
        assert_eq!(ino, 1);
        // No server round trip for dot names.
        assert!(store.listed().is_empty());
    }

    #[test]
    fn lookup_offline_failure_is_swallowed() {
        let store = seeded();
        store.fail_ensure_listed(StoreError::Io("down".into()));
        let (ino, _) = lookup_decision(&store, 7, 8, 2, OsStr::new("readme")).unwrap();
        assert_eq!(ino, 3);
    }

    #[test]
    fn getattr_unknown_inode_is_not_found() {
        let store = seeded();
        assert_eq!(
            getattr_decision(&store, 7, 8, 42).unwrap_err(),
            OpError::NotFound
        );
    }

    #[test]
    fn getattr_serves_cached_data_when_refresh_fails() {
        let store = seeded();
        store.fail_refresh();
        let attr = getattr_decision(&store, 7, 8, 3).unwrap();
        assert_eq!(attr.ino, INodeNo(3));
    }

    #[test]
    fn getattr_reports_refreshed_metadata() {
        use crate::store::db::attributes::{Access, FileAttrData, Stat};
        let store = MockStore::new();
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
