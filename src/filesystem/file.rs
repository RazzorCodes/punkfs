//! File content I/O: open, read, write, flush, release, fsync.
//!
//! Lifecycle of an open file, as driven by e.g. `cat file`:
//! `lookup` (resolve path) -> `open` -> `read` (maybe repeatedly) ->
//! `flush` (on close) -> `release`. Each op resolves its inode to a path
//! and hands off to the store; the content rpcs don't exist yet, so the
//! store answers `Unimplemented` and callers see `ENOSYS`.
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

use super::Fs;
use super::error::OpResult;
use super::helpers;
use crate::store::StoreApi;

/// Open file `ino`; returns the handle later ops must present.
pub(crate) fn open_decision(
    store: &dyn StoreApi,
    ino: u64,
    flags: OpenFlags,
) -> OpResult<u64> {
    let path = helpers::path_of(store, ino)?;
    Ok(store.open(&path, flags)?)
}

/// Read up to `size` bytes of `ino` at `offset`. A short result means EOF.
pub(crate) fn read_decision(
    store: &dyn StoreApi,
    ino: u64,
    fh: u64,
    offset: u64,
    size: u32,
) -> OpResult<Vec<u8>> {
    let path = helpers::path_of(store, ino)?;
    Ok(store.read(&path, fh, offset, size)?)
}

/// Write `data` at `offset`; returns the number of bytes accepted.
pub(crate) fn write_decision(
    store: &dyn StoreApi,
    ino: u64,
    fh: u64,
    offset: u64,
    data: &[u8],
) -> OpResult<u32> {
    let path = helpers::path_of(store, ino)?;
    Ok(store.write(&path, fh, offset, data)?)
}

/// Push this handle's buffered state to the owning node.
pub(crate) fn flush_decision(store: &dyn StoreApi, ino: u64, fh: u64) -> OpResult<()> {
    let path = helpers::path_of(store, ino)?;
    Ok(store.flush(&path, fh)?)
}

/// Drop the handle (once per `open`).
pub(crate) fn release_decision(store: &dyn StoreApi, ino: u64, fh: u64) -> OpResult<()> {
    let path = helpers::path_of(store, ino)?;
    Ok(store.release(&path, fh)?)
}

/// Durability barrier; `datasync` skips metadata.
pub(crate) fn fsync_decision(
    store: &dyn StoreApi,
    ino: u64,
    fh: u64,
    datasync: bool,
) -> OpResult<()> {
    let path = helpers::path_of(store, ino)?;
    Ok(store.fsync(&path, fh, datasync)?)
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
    fh: FileHandle,
    _lock_owner: LockOwner,
    reply: ReplyEmpty,
) {
    match flush_decision(fs.store.as_ref(), ino.0, fh.0) {
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
    fh: FileHandle,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    _flush: bool,
    reply: ReplyEmpty,
) {
    let _ = release_decision(fs.store.as_ref(), ino.0, fh.0);
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
    use crate::db::fstree::FsEntryKind;
    use crate::store::StoreError;
    use crate::store::mock::MockStore;

    fn seeded() -> MockStore {
        let store = MockStore::new();
        store.insert("/", 1, FsEntryKind::Directory);
        store.insert("/f", 2, FsEntryKind::File);
        store
    }

    #[test]
    fn open_forwards_resolved_path_and_flags() {
        let store = seeded();
        store.expect_open(Ok(41));
        let fh = open_decision(&store, 2, OpenFlags(0)).unwrap();
        assert_eq!(fh, 41);
        assert_eq!(store.calls().open, vec![("/f".to_string(), OpenFlags(0))]);
    }

    #[test]
    fn content_ops_are_unimplemented_today() {
        let store = seeded();
        assert_eq!(
            read_decision(&store, 2, 0, 0, 16).unwrap_err(),
            super::super::error::OpError::Unimplemented
        );
        assert_eq!(
            write_decision(&store, 2, 0, 0, b"x").unwrap_err(),
            super::super::error::OpError::Unimplemented
        );
        assert_eq!(
            fsync_decision(&store, 2, 0, false).unwrap_err(),
            super::super::error::OpError::Unimplemented
        );
    }

    #[test]
    fn read_returns_store_bytes() {
        let store = seeded();
        store.expect_read(Ok(b"hello".to_vec()));
        assert_eq!(read_decision(&store, 2, 41, 5, 5).unwrap(), b"hello");
        assert_eq!(store.calls().read, vec![("/f".to_string(), 41, 5, 5)]);
    }

    #[test]
    fn unknown_inode_never_reaches_the_store() {
        let store = seeded();
        let err = read_decision(&store, 99, 0, 0, 1).unwrap_err();
        assert_eq!(err, super::super::error::OpError::NotFound);
        assert!(store.calls().read.is_empty());
    }

    #[test]
    fn store_io_error_becomes_io() {
        let store = seeded();
        store.expect_flush(Err(StoreError::Io("boom".into())));
        let err = flush_decision(&store, 2, 0).unwrap_err();
        assert_eq!(err.errno(), fuser::Errno::EIO);
    }

    #[test]
    fn write_returns_short_count_from_store() {
        let store = seeded();
        store.expect_write(Ok(3));
        assert_eq!(write_decision(&store, 2, 41, 0, b"abc").unwrap(), 3);
        assert_eq!(
            store.calls().write,
            vec![("/f".to_string(), 41, 0, b"abc".to_vec())]
        );
    }

    #[test]
    fn flush_release_and_fsync_reach_the_store() {
        let store = seeded();
        store.expect_flush(Ok(()));
        store.expect_release(Ok(()));
        store.expect_fsync(Ok(()));

        flush_decision(&store, 2, 7).unwrap();
        release_decision(&store, 2, 7).unwrap();
        fsync_decision(&store, 2, 7, true).unwrap();

        let calls = store.calls();
        assert_eq!(calls.flush, vec![("/f".to_string(), 7)]);
        assert_eq!(calls.release, vec![("/f".to_string(), 7)]);
        assert_eq!(calls.fsync, vec![("/f".to_string(), 7, true)]);
    }

    #[test]
    fn open_error_from_store_is_propagated() {
        let store = seeded();
        store.expect_open(Err(StoreError::NotPermitted));
        let err = open_decision(&store, 2, OpenFlags(0)).unwrap_err();
        assert_eq!(err.errno(), fuser::Errno::EPERM);
    }
}
