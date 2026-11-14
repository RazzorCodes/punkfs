//! A `StoreApi` implementation for unit tests.
//!
//! Two jobs:
//! - **Fake catalog**: an in-memory set of entries with the same lookup
//!   semantics as the real `FsTree`, seeded by `insert`/`insert_with`.
//! - **Spy + scripted stubs**: every trait method appends to a [`Calls`]
//!   log so a test can assert what reached the store, and the mutating
//!   methods can be told what to answer via `expect_*`. Left unscripted
//!   they return `Unimplemented`, matching the real stubs.
//!
//! Not behind a feature flag: `#[cfg(test)]`-only, so it costs nothing in
//! release builds and cannot be used from the integration suite (which
//! goes through a real mount).
use std::sync::Mutex;

use fuser::OpenFlags;
use fuser::RenameFlags;

use crate::db::fstree::{FsEntry, FsEntryKind, FsMetadata};
use crate::store::{SetattrChanges, StatFs, StoreApi, StoreChild, StoreError};

/// One recorded call per trait method, in order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Calls {
    pub get_inode: Vec<String>,
    pub get_path: Vec<u64>,
    pub ensure_listed: Vec<String>,
    pub get_children: Vec<String>,
    pub open: Vec<(String, OpenFlags)>,
    pub read: Vec<(String, u64, u64, u32)>,
    pub write: Vec<(String, u64, u64, Vec<u8>)>,
    pub flush: Vec<(String, u64)>,
    pub release: Vec<(String, u64)>,
    pub fsync: Vec<(String, u64, bool)>,
    pub setattr: Vec<(String, SetattrChanges)>,
    pub mkdir: Vec<(String, u32)>,
    pub unlink: Vec<String>,
    pub rmdir: Vec<String>,
    pub rename: Vec<(String, String, RenameFlags)>,
    pub statfs: usize,
}

#[derive(Default)]
struct Expectations {
    open: Option<Result<u64, StoreError>>,
    read: Option<Result<Vec<u8>, StoreError>>,
    write: Option<Result<u32, StoreError>>,
    flush: Option<Result<(), StoreError>>,
    release: Option<Result<(), StoreError>>,
    fsync: Option<Result<(), StoreError>>,
    setattr: Option<Result<FsMetadata, StoreError>>,
    mkdir: Option<Result<u64, StoreError>>,
    statfs: Option<Result<StatFs, StoreError>>,
}

#[derive(Default)]
struct State {
    entries: Vec<FsEntry>,
    listed: Vec<String>,
    calls: Calls,
    expects: Expectations,
    fail_ensure_listed: Option<StoreError>,
    fail_get_children: bool,
}

impl State {
    fn find_path(&self, inode: u64) -> Option<&FsEntry> {
        self.entries.iter().find(|e| e.inode == inode)
    }

    fn find_inode(&self, path: &str) -> Option<&FsEntry> {
        self.entries.iter().find(|e| e.path == path)
    }
}

pub(crate) struct MockStore {
    state: Mutex<State>,
}

impl MockStore {
    pub(crate) fn new() -> Self {
        MockStore {
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    // --- seeding -------------------------------------------------------

    /// Add an entry with empty metadata.
    pub(crate) fn insert(&self, path: &str, inode: u64, kind: FsEntryKind) {
        self.insert_with(path, inode, kind, FsMetadata::default());
    }

    /// Add or replace an entry (matched by inode or path, like
    /// `FsTree::upsert_entry`).
    pub(crate) fn insert_with(
        &self,
        path: &str,
        inode: u64,
        kind: FsEntryKind,
        metadata: FsMetadata,
    ) {
        let mut state = self.lock();
        let entry = FsEntry {
            inode,
            path: path.to_owned(),
            kind,
            metadata,
            node_id: "mock-node".into(),
        };
        match state
            .entries
            .iter_mut()
            .find(|e| e.inode == inode || e.path == path)
        {
            Some(existing) => *existing = entry,
            None => state.entries.push(entry),
        }
    }

    // --- inspection ----------------------------------------------------

    /// Directories `ensure_listed` was asked about, in order.
    pub(crate) fn listed(&self) -> Vec<String> {
        self.lock().listed.clone()
    }

    /// The full recorded call log.
    pub(crate) fn calls(&self) -> Calls {
        self.lock().calls.clone()
    }

    // --- failure injection ---------------------------------------------

    /// Make every future `ensure_listed` fail with `err`.
    pub(crate) fn fail_ensure_listed(&self, err: StoreError) {
        self.lock().fail_ensure_listed = Some(err);
    }

    /// Make `get_children` return an empty listing (server down).
    pub(crate) fn fail_get_children(&self) {
        self.lock().fail_get_children = true;
    }

    // --- scripted stubs ------------------------------------------------

    pub(crate) fn expect_open(&self, result: Result<u64, StoreError>) {
        self.lock().expects.open = Some(result);
    }

    pub(crate) fn expect_read(&self, result: Result<Vec<u8>, StoreError>) {
        self.lock().expects.read = Some(result);
    }

    pub(crate) fn expect_write(&self, result: Result<u32, StoreError>) {
        self.lock().expects.write = Some(result);
    }

    pub(crate) fn expect_flush(&self, result: Result<(), StoreError>) {
        self.lock().expects.flush = Some(result);
    }

    pub(crate) fn expect_release(&self, result: Result<(), StoreError>) {
        self.lock().expects.release = Some(result);
    }

    pub(crate) fn expect_fsync(&self, result: Result<(), StoreError>) {
        self.lock().expects.fsync = Some(result);
    }

    pub(crate) fn expect_setattr(&self, result: Result<FsMetadata, StoreError>) {
        self.lock().expects.setattr = Some(result);
    }

    pub(crate) fn expect_mkdir(&self, result: Result<u64, StoreError>) {
        self.lock().expects.mkdir = Some(result);
    }

    pub(crate) fn expect_statfs(&self, result: Result<StatFs, StoreError>) {
        self.lock().expects.statfs = Some(result);
    }
}

impl StoreApi for MockStore {
    fn node_id(&self) -> &str {
        // The trait has no place to borrow the guard from, and the id is
        // fixed for the lifetime of the mock.
        "mock-node"
    }

    fn get_inode(&self, path: &str) -> Option<u64> {
        let mut state = self.lock();
        state.calls.get_inode.push(path.to_owned());
        state.find_inode(path).map(|e| e.inode)
    }

    fn get_path(&self, inode: u64) -> Option<String> {
        let mut state = self.lock();
        state.calls.get_path.push(inode);
        state.find_path(inode).map(|e| e.path.clone())
    }

    fn get_entry(&self, inode: u64) -> Option<FsEntry> {
        self.lock().find_path(inode).cloned()
    }

    fn ensure_listed(&self, dir: &str) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.ensure_listed.push(dir.to_owned());
        if let Some(err) = state.fail_ensure_listed.clone() {
            return Err(err);
        }
        state.listed.push(dir.to_owned());
        Ok(())
    }

    fn get_children(&self, path: &str) -> Vec<StoreChild> {
        let mut state = self.lock();
        state.calls.get_children.push(path.to_owned());
        if state.fail_get_children {
            return Vec::new();
        }

        let prefix = if path.ends_with('/') {
            path.to_owned()
        } else {
            format!("{path}/")
        };
        state
            .entries
            .iter()
            .filter(|e| {
                e.path
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
            })
            .map(|e| StoreChild {
                inode: e.inode,
                name: e.path.rsplit('/').next().unwrap_or("").to_owned(),
                kind: e.kind,
            })
            .collect()
    }

    fn open(&self, path: &str, flags: OpenFlags) -> Result<u64, StoreError> {
        let mut state = self.lock();
        state.calls.open.push((path.to_owned(), flags));
        state
            .expects
            .open
            .clone()
            .unwrap_or(Err(StoreError::Unimplemented))
    }

    fn read(&self, path: &str, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, StoreError> {
        let mut state = self.lock();
        state.calls.read.push((path.to_owned(), fh, offset, size));
        state
            .expects
            .read
            .clone()
            .unwrap_or(Err(StoreError::Unimplemented))
    }

    fn write(&self, path: &str, fh: u64, offset: u64, data: &[u8]) -> Result<u32, StoreError> {
        let mut state = self.lock();
        state
            .calls
            .write
            .push((path.to_owned(), fh, offset, data.to_vec()));
        state
            .expects
            .write
            .clone()
            .unwrap_or(Err(StoreError::Unimplemented))
    }

    fn flush(&self, path: &str, fh: u64) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.flush.push((path.to_owned(), fh));
        state
            .expects
            .flush
            .clone()
            .unwrap_or(Err(StoreError::Unimplemented))
    }

    fn release(&self, path: &str, fh: u64) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.release.push((path.to_owned(), fh));
        state
            .expects
            .release
            .clone()
            .unwrap_or(Err(StoreError::Unimplemented))
    }

    fn fsync(&self, path: &str, fh: u64, datasync: bool) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.fsync.push((path.to_owned(), fh, datasync));
        state
            .expects
            .fsync
            .clone()
            .unwrap_or(Err(StoreError::Unimplemented))
    }

    fn setattr(&self, path: &str, changes: SetattrChanges) -> Result<FsMetadata, StoreError> {
        let mut state = self.lock();
        state.calls.setattr.push((path.to_owned(), changes));
        state
            .expects
            .setattr
            .clone()
            .unwrap_or(Err(StoreError::Unimplemented))
    }

    fn mkdir(&self, path: &str, mode: u32) -> Result<u64, StoreError> {
        let mut state = self.lock();
        state.calls.mkdir.push((path.to_owned(), mode));
        state
            .expects
            .mkdir
            .clone()
            .unwrap_or(Err(StoreError::Unimplemented))
    }

    fn unlink(&self, path: &str) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.unlink.push(path.to_owned());
        Err(StoreError::Unimplemented)
    }

    fn rmdir(&self, path: &str) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.rmdir.push(path.to_owned());
        Err(StoreError::Unimplemented)
    }

    fn rename(&self, old: &str, new: &str, flags: RenameFlags) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.rename.push((old.to_owned(), new.to_owned(), flags));
        Err(StoreError::Unimplemented)
    }

    fn statfs(&self) -> Result<StatFs, StoreError> {
        let mut state = self.lock();
        state.calls.statfs += 1;
        state
            .expects
            .statfs
            .clone()
            .unwrap_or(Err(StoreError::Unimplemented))
    }
}
