//! A `StoreApi` implementation for unit tests.
//!
//! Two jobs:
//! - **Fake catalog**: an in-memory set of entries with the same lookup
//!   semantics as the real `MetadataCache`, seeded by `insert`/`insert_with`.
//! - **Spy + scripted stubs**: every trait method appends to a [`Calls`]
//!   log so a test can assert what reached the store, and the fallible
//!   methods can be told what to answer via `expect_*`. Left unscripted
//!   they return `Unsupported`, matching a backend that cannot do it.
//!
//! `#[cfg(test)]`-only: it costs nothing in release builds.
use std::sync::Mutex;

use fuser::RenameFlags;

use crate::store::db::attributes::{FileAttrData, FsEntryKind};
use crate::store::db::metadata_cache::{Availability, FsEntry};
use crate::store::{SetattrChanges, StatFs, StoreApi, StoreChild, StoreError};

/// One recorded call per trait method, in order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Calls {
    pub get_inode: Vec<String>,
    pub get_path: Vec<u64>,
    pub ensure_listed: Vec<String>,
    pub get_children: Vec<String>,
    pub read: Vec<(String, u64, u32)>,
    pub write: Vec<(String, u64, Vec<u8>)>,
    pub fsync: Vec<(String, bool)>,
    pub setattr: Vec<(String, SetattrChanges)>,
    pub refresh: Vec<String>,
    pub statfs: usize,
    pub unlink: Vec<String>,
    pub rmdir: Vec<String>,
    pub rename: Vec<(String, String, RenameFlags)>,
}

#[derive(Default)]
struct Expectations {
    read: Option<Result<Vec<u8>, StoreError>>,
    write: Option<Result<u32, StoreError>>,
    statfs: Option<Result<StatFs, StoreError>>,
    fail_refresh: bool,
}

#[derive(Default)]
struct State {
    entries: Vec<FsEntry>,
    listed: Vec<String>,
    calls: Calls,
    expects: Expectations,
    fail_ensure_listed: Option<StoreError>,
    fail_get_children: bool,
    next_inode: u64,
}

impl State {
    fn find_inode(&self, inode: u64) -> Option<&FsEntry> {
        self.entries.iter().find(|e| e.inode == inode)
    }

    fn find_path(&self, path: &str) -> Option<&FsEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    fn alloc(&mut self) -> u64 {
        self.next_inode = self.next_inode.max(2) + 1;
        self.next_inode - 1
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

    /// Add an entry with default attributes but the given kind.
    pub(crate) fn insert(&self, path: &str, inode: u64, kind: FsEntryKind) {
        let guid = format!("guid-{path}");
        let attr = FileAttrData {
            guid: guid.clone(),
            kind,
            ..FileAttrData::default()
        };
        self.insert_with(path, inode, kind, attr);
    }

    /// Add or replace an entry (matched by inode or path).
    pub(crate) fn insert_with(&self, path: &str, inode: u64, kind: FsEntryKind, attr: FileAttrData) {
        let mut state = self.lock();
        let inode = if inode == 0 { state.alloc() } else { inode };
        let entry = FsEntry {
            inode,
            path: path.to_owned(),
            guid: attr.guid.clone(),
            kind,
            node_id: "mock-node".to_owned(),
            availability: Availability::Available,
            attr,
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

    /// Make `refresh` fail, so a test can check the cached fallback.
    pub(crate) fn fail_refresh(&self) {
        self.lock().expects.fail_refresh = true;
    }

    // --- scripted stubs ------------------------------------------------

    pub(crate) fn expect_read(&self, result: Result<Vec<u8>, StoreError>) {
        self.lock().expects.read = Some(result);
    }

    pub(crate) fn expect_write(&self, result: Result<u32, StoreError>) {
        self.lock().expects.write = Some(result);
    }

    pub(crate) fn expect_statfs(&self, result: Result<StatFs, StoreError>) {
        self.lock().expects.statfs = Some(result);
    }
}

impl StoreApi for MockStore {
    fn node_id(&self) -> &str {
        "mock-node"
    }

    fn get_inode(&self, path: &str) -> Option<u64> {
        let mut state = self.lock();
        state.calls.get_inode.push(path.to_owned());
        state.find_path(path).map(|e| e.inode)
    }

    fn get_path(&self, inode: u64) -> Option<String> {
        let mut state = self.lock();
        state.calls.get_path.push(inode);
        state.find_inode(inode).map(|e| e.path.clone())
    }

    fn owner_of(&self, path: &str) -> Option<String> {
        self.lock().find_path(path).map(|e| e.node_id.clone())
    }

    fn get_entry(&self, inode: u64) -> Option<FsEntry> {
        self.lock().find_inode(inode).cloned()
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

    fn read(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>, StoreError> {
        let mut state = self.lock();
        state.calls.read.push((path.to_owned(), offset, size));
        state
            .expects
            .read
            .clone()
            .unwrap_or(Err(StoreError::Unsupported("read".into())))
    }

    fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<u32, StoreError> {
        let mut state = self.lock();
        state.calls.write.push((path.to_owned(), offset, data.to_vec()));
        state
            .expects
            .write
            .clone()
            .unwrap_or(Err(StoreError::Unsupported("write".into())))
    }

    fn fsync(&self, path: &str, datasync: bool) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.fsync.push((path.to_owned(), datasync));
        Err(StoreError::Unsupported("fsync".into()))
    }

    fn setattr(&self, path: &str, changes: SetattrChanges) -> Result<FileAttrData, StoreError> {
        let mut state = self.lock();
        state.calls.setattr.push((path.to_owned(), changes));
        Err(StoreError::Unsupported("setattr".into()))
    }

    fn refresh(&self, path: &str) -> Result<FileAttrData, StoreError> {
        let mut state = self.lock();
        state.calls.refresh.push(path.to_owned());
        if state.expects.fail_refresh {
            return Err(StoreError::Io("refresh failed".into()));
        }
        state
            .find_path(path)
            .map(|e| e.attr.clone())
            .ok_or(StoreError::NotFound)
    }

    fn statfs(&self) -> Result<StatFs, StoreError> {
        let mut state = self.lock();
        state.calls.statfs += 1;
        state
            .expects
            .statfs
            .clone()
            .unwrap_or(Err(StoreError::Unsupported("statfs".into())))
    }

    fn unlink(&self, path: &str) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.unlink.push(path.to_owned());
        let pos = state
            .entries
            .iter()
            .position(|e| e.path == path)
            .ok_or(StoreError::NotFound)?;
        state.entries.remove(pos);
        Ok(())
    }

    fn rmdir(&self, path: &str) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.calls.rmdir.push(path.to_owned());
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let pos = state
            .entries
            .iter()
            .position(|e| e.path == path)
            .ok_or(StoreError::NotFound)?;
        if state.entries.iter().any(|e| e.path.starts_with(&prefix)) {
            return Err(StoreError::Io("directory not empty".into()));
        }
        state.entries.remove(pos);
        Ok(())
    }

    fn rename(&self, old: &str, new: &str, flags: RenameFlags) -> Result<(), StoreError> {
        let mut state = self.lock();
        state
            .calls
            .rename
            .push((old.to_owned(), new.to_owned(), flags));

        let prefix = format!("{}/", old.trim_end_matches('/'));
        if !state.entries.iter().any(|e| e.path == old) {
            return Err(StoreError::NotFound);
        }
        for entry in &mut state.entries {
            if entry.path == old {
                entry.path = new.to_owned();
            } else if let Some(rest) = entry.path.strip_prefix(&prefix) {
                entry.path = format!("{}/{rest}", new.trim_end_matches('/'));
            }
        }
        Ok(())
    }
}
