//! File content I/O: open, read, write, flush, release, fsync.
//!
//! Lifecycle of an open file, as driven by e.g. `cat file`:
//! `lookup` (resolve path) -> `open` -> `read` (maybe repeatedly) ->
//! `flush` (on close) -> `release`.
//!
//! Two properties worth stating because they shape this module:
//!
//! - **No remote creation.** `open` requires an existing catalog entry;
//!   local creation is handled separately by the passthrough layer.
//! - **Positional I/O.** The server's read/write rpcs take an explicit
//!   offset and keep no per-handle cursor, so the local handle is bookkeeping
//!   only and there is nothing to leak.
use fuser::FileHandle;
use fuser::FopenFlags;
use fuser::INodeNo;
use fuser::LockOwner;
use fuser::OpenFlags;
use fuser::ReplyData;
use fuser::ReplyEmpty;
use fuser::ReplyOpen;
use fuser::ReplyWrite;
use fuser::Request;
use fuser::WriteFlags;

use super::error::{OpError, OpResult};
use super::helpers;
use crate::core::punkfs::Fs;
use crate::store::StoreApi;

/// Open file `ino`; returns the handle later ops must present.
///
/// Refuses to conjure an entry that does not exist: `O_CREAT` does not
/// imply creation here, so a missing path is `ENOENT` whether or not the
/// flag is set.
pub(crate) fn open_decision(store: &dyn StoreApi, ino: u64, _flags: OpenFlags) -> OpResult<u64> {
    let path = helpers::path_of(store, ino)?;
    // Confirms the entry is still known to the catalog before we hand out a
    // handle; the handle itself is positional and carries no server state.
    let _ = store.refresh(&path).map_err(|e| match e {
        crate::store::StoreError::NotFound => OpError::NotFound,
        other => OpError::from(other),
    })?;
    Ok(ino)
}

/// Read up to `size` bytes of `ino` at `offset`. A short result means EOF.
pub(crate) fn read_decision(
    store: &dyn StoreApi,
    ino: u64,
    _fh: u64,
    offset: u64,
    size: u32,
) -> OpResult<Vec<u8>> {
    let path = helpers::path_of(store, ino)?;
    Ok(store.read(&path, offset, size)?)
}

/// Write `data` at `offset`; returns the number of bytes accepted.
pub(crate) fn write_decision(
    store: &dyn StoreApi,
    ino: u64,
    _fh: u64,
    offset: u64,
    data: &[u8],
) -> OpResult<u32> {
    let path = helpers::path_of(store, ino)?;
    Ok(store.write(&path, offset, data)?)
}

/// Push this handle's buffered state to the owning node.
///
/// There is no client-side buffering — writes go straight out — so this is
/// a no-op beyond confirming the entry still exists.
pub(crate) fn flush_decision(store: &dyn StoreApi, ino: u64) -> OpResult<()> {
    let _ = helpers::path_of(store, ino)?;
    Ok(())
}

/// Drop the handle (once per `open`). Nothing to release on the server.
pub(crate) fn release_decision(store: &dyn StoreApi, ino: u64) -> OpResult<()> {
    let _ = helpers::path_of(store, ino)?;
    Ok(())
}

/// Durability barrier; `datasync` skips metadata.
pub(crate) fn fsync_decision(
    store: &dyn StoreApi,
    ino: u64,
    _fh: u64,
    datasync: bool,
) -> OpResult<()> {
    let path = helpers::path_of(store, ino)?;
    Ok(store.fsync(&path, datasync)?)
}

pub(crate) fn open(fs: &Fs, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
    match open_decision(fs.store.as_ref(), ino.0, flags) {
        Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn read(
    fs: &Fs,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    size: u32,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyData,
) {
    match read_decision(fs.store.as_ref(), ino.0, fh.0, offset, size) {
        Ok(data) => reply.data(&data),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn write(
    fs: &Fs,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    data: &[u8],
    _write_flags: WriteFlags,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyWrite,
) {
    match write_decision(fs.store.as_ref(), ino.0, fh.0, offset, data) {
        Ok(written) => reply.written(written),
        Err(e) => reply.error(e.errno()),
    }
}

pub(crate) fn flush(
    fs: &Fs,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    _lock_owner: LockOwner,
    reply: ReplyEmpty,
) {
    match flush_decision(fs.store.as_ref(), ino.0) {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e.errno()),
    }
}

/// Errors from `release` are never delivered to the application (the
/// close/munmap already returned), so an unusable store still replies ok.
pub(crate) fn release(
    fs: &Fs,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    _flush: bool,
    reply: ReplyEmpty,
) {
    let _ = release_decision(fs.store.as_ref(), ino.0);
    reply.ok();
}

pub(crate) fn fsync(
    fs: &Fs,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    datasync: bool,
    reply: ReplyEmpty,
) {
    match fsync_decision(fs.store.as_ref(), ino.0, fh.0, datasync) {
        Ok(()) => reply.ok(),
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
        store.insert("/f", 2, FsEntryKind::File);
        store
    }

    #[test]
    fn open_returns_the_inode_as_the_handle() {
        let store = seeded();
        let fh = open_decision(&store, 2, OpenFlags(0)).unwrap();
        assert_eq!(fh, 2);
        assert_eq!(store.calls().refresh, vec!["/f".to_string()]);
    }

    #[test]
    fn open_does_not_create_missing_entries() {
        let store = seeded();
        assert_eq!(
            open_decision(&store, 99, OpenFlags(0)).unwrap_err(),
            OpError::NotFound
        );
    }

    #[test]
    fn read_forwards_offset_and_size() {
        let store = seeded();
        store.expect_read(Ok(b"hello".to_vec()));
        assert_eq!(read_decision(&store, 2, 2, 5, 5).unwrap(), b"hello");
        assert_eq!(store.calls().read, vec![("/f".to_string(), 5, 5)]);
    }

    #[test]
    fn read_error_is_propagated() {
        let store = seeded();
        store.expect_read(Err(StoreError::Io("boom".into())));
        assert_eq!(
            read_decision(&store, 2, 2, 0, 1).unwrap_err().errno(),
            fuser::Errno::EIO
        );
    }

    #[test]
    fn unknown_inode_never_reaches_the_store() {
        let store = seeded();
        let err = read_decision(&store, 99, 0, 0, 1).unwrap_err();
        assert_eq!(err, OpError::NotFound);
        assert!(store.calls().read.is_empty());
    }

    #[test]
    fn write_returns_the_count_from_the_store() {
        let store = seeded();
        store.expect_write(Ok(3));
        assert_eq!(write_decision(&store, 2, 2, 0, b"abc").unwrap(), 3);
        assert_eq!(
            store.calls().write,
            vec![("/f".to_string(), 0, b"abc".to_vec())]
        );
    }

    #[test]
    fn write_refusal_surfaces_as_a_recoverable_error() {
        let store = seeded();
        store.expect_write(Err(StoreError::NotPermitted));
        assert_eq!(
            write_decision(&store, 2, 2, 0, b"x").unwrap_err(),
            OpError::NotPermitted
        );
    }

    #[test]
    fn fsync_forwards_datasync() {
        let store = seeded();
        store.expect_fsync(Ok(()));
        fsync_decision(&store, 2, 2, true).unwrap();
        assert_eq!(store.calls().fsync, vec![("/f".to_string(), true)]);
    }

    #[test]
    fn flush_and_release_are_local_noops() {
        let store = seeded();
        flush_decision(&store, 2).unwrap();
        release_decision(&store, 2).unwrap();
        assert_eq!(
            flush_decision(&store, 99).unwrap_err(),
            OpError::NotFound,
            "an unknown inode is still reported"
        );
    }
}
