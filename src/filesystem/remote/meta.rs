//! Metadata and namespace-mutating operations: setattr, unlink, rmdir,
//! rename, statfs.
//!
//! Namespace creation is handled by the local passthrough layer. Attribute
//! changes are routed to the owning backing store.
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
/// Handle-based identity guarantees, including `ftruncate` after rename or
/// unlink, remain deferred.
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
    if ctime.is_some()
        || crtime.is_some()
        || chgtime.is_some()
        || bkuptime.is_some()
        || flags.is_some()
    {
        return Err(OpError::Unsupported);
    }
    let split_time = |time: Option<TimeOrNow>| -> OpResult<(Option<i64>, Option<u32>)> {
        let Some(time) = time else {
            return Ok((None, None));
        };
        let time = match time {
            TimeOrNow::SpecificTime(t) => t,
            TimeOrNow::Now => SystemTime::now(),
        };
        // fuser 0.18 decodes a negative timespec as epoch - (|sec| +
        // nsec), instead of epoch + sec + nsec. Invert that conversion
        // here; remove this workaround when upgrading to fuser >= 0.19.
        if let Err(e) = time.duration_since(SystemTime::UNIX_EPOCH) {
            let d = e.duration();
            let secs = i64::try_from(d.as_secs()).map_err(|_| OpError::InvalidInput)?;
            return Ok((Some(-secs), Some(d.subsec_nanos())));
        }
        let d = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("negative time handled above");
        Ok((
            Some(i64::try_from(d.as_secs()).map_err(|_| OpError::InvalidInput)?),
            Some(d.subsec_nanos()),
        ))
    };
    let (accessed_unix, accessed_nanos) = split_time(atime)?;
    let (modified_unix, modified_nanos) = split_time(mtime)?;
    let result = store.setattr(
        &path,
        crate::store::SetattrChanges {
            mode,
            uid: uid_change,
            gid: gid_change,
            size,
            accessed_unix,
            modified_unix,
            accessed_nanos,
            modified_nanos,
        },
    );
    if let (Some(ctx), Some(logger)) = (store.operation_context(), crate::telemetry::global()) {
        let severity = if result.is_ok() {
            crate::telemetry::Severity::Debug
        } else {
            crate::telemetry::Severity::Warning
        };
        if logger.enabled(severity) {
            let mut event = crate::telemetry::Event::new(
                severity,
                "filesystem.remote",
                "attribute operation completed",
            );
            event.operation = Some("setattr".into());
            event.fields.insert(
                "outcome".into(),
                serde_json::json!(if result.is_ok() { "ok" } else { "error" }),
            );
            logger.emit_in(ctx, event);
        }
    }
    Ok(helpers::attr_from(ino, &result?, uid, gid))
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

    if source.kind != FsEntryKind::Directory || flags.contains(RenameFlags::RENAME_EXCHANGE) {
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            store.ensure_listed(&new_parent)?;
            if store.has_conflict(&old_path) || store.has_conflict(&new_path) {
                return Err(OpError::Local(libc::EIO));
            }
        } else {
            let _ = store.ensure_listed(&new_parent);
        }
        let owner = store.owner_of(&old_path).ok_or(OpError::NotFound)?;
        match store.owner_of(&new_path) {
            // Renaming in place on the same node.
            Some(dest_owner) if dest_owner == owner => {}
            // The destination is new: it is created on the source's node.
            None if flags.contains(RenameFlags::RENAME_EXCHANGE) => return Err(OpError::NotFound),
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

    /// unlink/rmdir join parent+name into a path and delegate to the store;
    /// a non-empty directory is refused locally before the store is asked.
    #[test]
    fn unlink_and_rmdir_join_paths_and_check_emptiness() {
        let store = seeded();
        unlink_decision(&store, 2, OsStr::new("a.txt")).unwrap();
        assert_eq!(store.calls().unlink, vec!["/docs/a.txt".to_string()]);

        let nonempty = seeded();
        let err = rmdir_decision(&nonempty, 1, OsStr::new("docs")).unwrap_err();
        assert_eq!(err, OpError::NotEmpty);
        assert!(nonempty.calls().rmdir.is_empty());

        let empty = seeded();
        rmdir_decision(&empty, 2, OsStr::new("empty")).unwrap();
        assert_eq!(empty.calls().rmdir, vec!["/docs/empty".to_string()]);

        assert_eq!(
            unlink_decision(&seeded(), 99, OsStr::new("x")).unwrap_err(),
            OpError::NotFound
        );
    }

    /// Rename joins both sides and refuses a move that would cross backing
    /// nodes (EXDEV); flags must pass through to the store unchanged.
    #[test]
    fn rename_joins_both_sides_and_refuses_cross_node_moves() {
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

        let flags = RenameFlags::RENAME_NOREPLACE;
        let passthrough = seeded();
        rename_decision(
            &passthrough,
            2,
            OsStr::new("a.txt"),
            2,
            OsStr::new("b"),
            flags,
        )
        .unwrap();
        assert_eq!(passthrough.calls().rename[0].2, flags);
    }

    /// Reading-only capacity counters are passed through as-is and a store
    /// failure maps to EIO.
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

        store.expect_statfs(Err(StoreError::Io("down".into())));
        assert_eq!(
            statfs_decision(&store).unwrap_err().errno(),
            fuser::Errno::EIO
        );
    }
}
