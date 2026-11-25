//! Persistent `redb`-backed catalog keyed by normalized UTF-8 relative path.
//!
//! Stores:
//! - `entries`: normalized path (`""` for root, `"dir/file"` for children) -> serialized metadata
//! - `path_to_inode`: normalized path -> constant local `u64` FUSE inode
//! - `inode_to_path`: constant local `u64` FUSE inode -> normalized path
//! - `meta`: `"next_inode"` -> monotonic `u64` counter (never reused after unlink)

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::core::metadata::{Availability, FsEntry};
use crate::core::types::{Access, FileAttrData, FileTimes, FsEntryKind, Stat};

const ENTRIES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("entries");
const PATH_TO_INODE_TABLE: TableDefinition<&str, u64> = TableDefinition::new("path_to_inode");
const INODE_TO_PATH_TABLE: TableDefinition<u64, &str> = TableDefinition::new("inode_to_path");
const META_TABLE: TableDefinition<&str, u64> = TableDefinition::new("meta");
const NEXT_INODE_KEY: &str = "next_inode";

/// Normalize a logical path into a UTF-8 relative key (`""` for root,
/// `"a/b"` for `/a/b`).
pub fn normalize_rel_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        trimmed.to_owned()
    }
}

/// Convert a normalized relative key back into a leading-slash logical path.
pub fn to_logical_path(rel: &str) -> String {
    if rel.is_empty() {
        "/".to_owned()
    } else {
        format!("/{rel}")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedTime {
    secs: i64,
    nanos: u32,
}

impl From<SystemTime> for PersistedTime {
    fn from(value: SystemTime) -> Self {
        match value.duration_since(UNIX_EPOCH) {
            Ok(d) => Self {
                secs: d.as_secs() as i64,
                nanos: d.subsec_nanos(),
            },
            Err(err) => {
                let d = err.duration();
                let secs = d.as_secs() as i64;
                let nanos = d.subsec_nanos();
                if nanos == 0 {
                    Self {
                        secs: -secs,
                        nanos: 0,
                    }
                } else {
                    Self {
                        secs: -secs - 1,
                        nanos: 1_000_000_000 - nanos,
                    }
                }
            }
        }
    }
}

impl From<PersistedTime> for SystemTime {
    fn from(value: PersistedTime) -> Self {
        let base = if value.secs >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(value.secs as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(value.secs.unsigned_abs()))
        };
        base.and_then(|t| t.checked_add(Duration::from_nanos(value.nanos as u64)))
            .unwrap_or(UNIX_EPOCH)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum PersistedKind {
    Unknown,
    File,
    Directory,
    Link,
    Fifo,
    Socket,
    BlockDevice,
    CharDevice,
}

impl From<FsEntryKind> for PersistedKind {
    fn from(kind: FsEntryKind) -> Self {
        match kind {
            FsEntryKind::Unknown => Self::Unknown,
            FsEntryKind::File => Self::File,
            FsEntryKind::Directory => Self::Directory,
            FsEntryKind::Link => Self::Link,
            FsEntryKind::Fifo => Self::Fifo,
            FsEntryKind::Socket => Self::Socket,
            FsEntryKind::BlockDevice => Self::BlockDevice,
            FsEntryKind::CharDevice => Self::CharDevice,
        }
    }
}

impl From<PersistedKind> for FsEntryKind {
    fn from(kind: PersistedKind) -> Self {
        match kind {
            PersistedKind::Unknown => Self::Unknown,
            PersistedKind::File => Self::File,
            PersistedKind::Directory => Self::Directory,
            PersistedKind::Link => Self::Link,
            PersistedKind::Fifo => Self::Fifo,
            PersistedKind::Socket => Self::Socket,
            PersistedKind::BlockDevice => Self::BlockDevice,
            PersistedKind::CharDevice => Self::CharDevice,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedEntry {
    inode: u64,
    rel_path: String,
    guid: String,
    kind: PersistedKind,
    node_id: String,
    /// Additional owners for merged directories.
    #[serde(default)]
    dir_owners: Vec<String>,
    available: bool,
    accessed: PersistedTime,
    modified: PersistedTime,
    changed: PersistedTime,
    created: PersistedTime,
    mode: u16,
    uid: u32,
    gid: u32,
    size: u64,
    blocks: u32,
    block_size: u16,
    rdev: u32,
    nlink: u32,
    flags: u32,
}

impl PersistedEntry {
    fn from_entry(entry: &FsEntry, dir_owners: Vec<String>) -> Self {
        Self {
            inode: entry.inode,
            rel_path: normalize_rel_path(&entry.path),
            guid: entry.guid.clone(),
            kind: entry.kind.into(),
            node_id: entry.node_id.clone(),
            dir_owners,
            available: entry.availability == Availability::Available,
            accessed: entry.attr.times.accessed.into(),
            modified: entry.attr.times.modified.into(),
            changed: entry.attr.times.changed.into(),
            created: entry.attr.times.created.into(),
            mode: entry.attr.access.mode,
            uid: entry.attr.access.uid,
            gid: entry.attr.access.gid,
            size: entry.attr.stat.size,
            blocks: entry.attr.stat.blocks,
            block_size: entry.attr.stat.block_size,
            rdev: entry.attr.stat.rdev,
            nlink: entry.attr.stat.nlink,
            flags: entry.attr.stat.flags,
        }
    }

    fn into_entry(self) -> FsEntry {
        let kind: FsEntryKind = self.kind.into();
        FsEntry {
            inode: self.inode,
            path: to_logical_path(&self.rel_path),
            guid: self.guid.clone(),
            kind,
            node_id: self.node_id,
            availability: if self.available {
                Availability::Available
            } else {
                Availability::Inaccessible
            },
            attr: FileAttrData {
                guid: self.guid,
                kind,
                times: FileTimes {
                    accessed: self.accessed.into(),
                    modified: self.modified.into(),
                    changed: self.changed.into(),
                    created: self.created.into(),
                },
                access: Access {
                    mode: self.mode,
                    uid: self.uid,
                    gid: self.gid,
                },
                stat: Stat {
                    size: self.size,
                    blocks: self.blocks,
                    block_size: self.block_size,
                    rdev: self.rdev,
                    nlink: self.nlink,
                    flags: self.flags,
                },
            },
        }
    }
}

/// One logical catalog record. A directory may represent several backings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogRow {
    pub entry: FsEntry,
    pub directory_owners: Vec<String>,
}

impl From<PersistedEntry> for CatalogRow {
    fn from(record: PersistedEntry) -> Self {
        let mut directory_owners = record.dir_owners.clone();
        if record.kind == PersistedKind::Directory
            && !record.node_id.is_empty()
            && !directory_owners.contains(&record.node_id)
        {
            directory_owners.push(record.node_id.clone());
        }
        Self {
            entry: record.into_entry(),
            directory_owners,
        }
    }
}

/// Changes to the path and inode indexes committed as a single transaction.
/// A `None` row is a deletion; new rows already have cache-assigned inodes.
pub(crate) type CatalogPatch = BTreeMap<String, Option<CatalogRow>>;

fn decode_row(bytes: &[u8]) -> Result<CatalogRow, redb::Error> {
    let record: PersistedEntry = serde_json::from_slice(bytes).map_err(storage_error)?;
    Ok(record.into())
}

fn storage_error(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> redb::StorageError {
    redb::StorageError::Io(std::io::Error::other(error))
}

pub struct PersistentDb {
    db: Database,
}

impl PersistentDb {
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(redb::StorageError::Io)?;
        }
        let db = Database::create(path)?;
        let write_txn = db.begin_write()?;
        {
            let _ = write_txn.open_table(ENTRIES_TABLE)?;
            let _ = write_txn.open_table(PATH_TO_INODE_TABLE)?;
            let _ = write_txn.open_table(INODE_TO_PATH_TABLE)?;
            let mut meta = write_txn.open_table(META_TABLE)?;
            let has_next = meta.get(NEXT_INODE_KEY)?.is_some();
            if !has_next {
                meta.insert(NEXT_INODE_KEY, 2u64)?;
            }
        }
        write_txn.commit()?;
        Ok(Self { db })
    }

    pub(crate) fn get_row(&self, path: &str) -> Result<Option<CatalogRow>, redb::Error> {
        let rel = normalize_rel_path(path);
        let txn = self.db.begin_read()?;
        let table = txn.open_table(ENTRIES_TABLE)?;
        table
            .get(rel.as_str())?
            .map(|g| decode_row(g.value()))
            .transpose()
    }

    pub(crate) fn rows_under(
        &self,
        path: &str,
        children_only: bool,
    ) -> Result<Vec<CatalogRow>, redb::Error> {
        let rel = normalize_rel_path(path);
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
        let txn = self.db.begin_read()?;
        let table = txn.open_table(ENTRIES_TABLE)?;
        let mut rows = Vec::new();
        if !children_only && let Some(row) = table.get(rel.as_str())? {
            rows.push(decode_row(row.value())?);
        }
        for item in table.range(prefix.as_str()..)? {
            let (key, value) = item?;
            let Some(suffix) = key.value().strip_prefix(&prefix) else {
                break;
            };
            if !suffix.is_empty() && (!children_only || !suffix.contains('/')) {
                rows.push(decode_row(value.value())?);
            }
        }
        Ok(rows)
    }

    pub(crate) fn apply_patch(&self, patch: &CatalogPatch) -> Result<(), redb::Error> {
        let txn = self.db.begin_write()?;
        {
            let mut entries = txn.open_table(ENTRIES_TABLE)?;
            let mut p2i = txn.open_table(PATH_TO_INODE_TABLE)?;
            let mut i2p = txn.open_table(INODE_TO_PATH_TABLE)?;
            let mut meta = txn.open_table(META_TABLE)?;
            let mut next = meta.get(NEXT_INODE_KEY)?.map_or(2, |value| value.value());
            // Retire all previous bindings before inserting any new ones;
            // rename/exchange can move an existing inode to another patch key.
            for path in patch.keys() {
                let rel = normalize_rel_path(path);
                if let Some(inode) = p2i.remove(rel.as_str())? {
                    i2p.remove(inode.value())?;
                }
                entries.remove(rel.as_str())?;
            }
            for (path, row) in patch {
                let Some(row) = row else { continue };
                let rel = normalize_rel_path(path);
                let inode = row.entry.inode;
                if inode == 0 || i2p.get(inode)?.is_some() {
                    return Err(storage_error("duplicate or invalid inode in catalog patch").into());
                }
                let record = PersistedEntry::from_entry(&row.entry, row.directory_owners.clone());
                let bytes = serde_json::to_vec(&record).map_err(storage_error)?;
                entries.insert(rel.as_str(), bytes.as_slice())?;
                p2i.insert(rel.as_str(), inode)?;
                i2p.insert(inode, rel.as_str())?;
                next = next.max(inode + 1);
            }
            meta.insert(NEXT_INODE_KEY, next)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn next_inode(&self) -> Result<u64, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let meta = read_txn.open_table(META_TABLE)?;
        Ok(meta
            .get(NEXT_INODE_KEY)?
            .map(|v| v.value())
            .unwrap_or(2)
            .max(2))
    }

    pub fn load_all(&self) -> Result<(Vec<FsEntry>, u64), redb::Error> {
        self.load_limit(None)
    }

    pub fn load_limit(&self, limit: Option<usize>) -> Result<(Vec<FsEntry>, u64), redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ENTRIES_TABLE)?;
        let meta = read_txn.open_table(META_TABLE)?;
        let next_inode = meta
            .get(NEXT_INODE_KEY)?
            .map(|v| v.value())
            .unwrap_or(2)
            .max(2);
        let mut entries = Vec::new();
        for item in table.iter()? {
            if limit.is_some_and(|max| entries.len() >= max) {
                break;
            }
            let (_, value) = item?;
            let record = serde_json::from_slice::<PersistedEntry>(value.value())
                .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
            entries.push(record.into_entry());
        }
        Ok((entries, next_inode))
    }

    pub fn get_by_path(&self, path: &str) -> Result<Option<FsEntry>, redb::Error> {
        let key = normalize_rel_path(path);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ENTRIES_TABLE)?;
        let Some(guard) = table.get(key.as_str())? else {
            return Ok(None);
        };
        let record = serde_json::from_slice::<PersistedEntry>(guard.value())
            .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
        Ok(Some(record.into_entry()))
    }

    /// Find an owner's entry by stable GUID, even if its path was evicted.
    pub fn get_by_guid(&self, owner: &str, guid: &str) -> Result<Option<FsEntry>, redb::Error> {
        if guid.is_empty() {
            return Ok(None);
        }
        let txn = self.db.begin_read()?;
        let entries = txn.open_table(ENTRIES_TABLE)?;
        for item in entries.iter()? {
            let (_, value) = item?;
            let record: PersistedEntry = serde_json::from_slice(value.value())
                .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
            if record.node_id == owner && record.guid == guid {
                return Ok(Some(record.into_entry()));
            }
        }
        Ok(None)
    }

    pub fn get_inode(&self, path: &str) -> Result<Option<u64>, redb::Error> {
        let key = normalize_rel_path(path);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(PATH_TO_INODE_TABLE)?;
        Ok(table.get(key.as_str())?.map(|v| v.value()))
    }

    pub fn get_path(&self, inode: u64) -> Result<Option<String>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(INODE_TO_PATH_TABLE)?;
        Ok(table.get(inode)?.map(|v| to_logical_path(v.value())))
    }

    pub fn get_by_inode(&self, inode: u64) -> Result<Option<FsEntry>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let i2p = read_txn.open_table(INODE_TO_PATH_TABLE)?;
        let Some(path_guard) = i2p.get(inode)? else {
            return Ok(None);
        };
        let rel = path_guard.value().to_owned();
        let entries = read_txn.open_table(ENTRIES_TABLE)?;
        let Some(guard) = entries.get(rel.as_str())? else {
            return Ok(None);
        };
        let record = serde_json::from_slice::<PersistedEntry>(guard.value())
            .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
        Ok(Some(record.into_entry()))
    }

    pub fn get_children_of(&self, path: &str) -> Result<Vec<FsEntry>, redb::Error> {
        let parent_rel = normalize_rel_path(path);
        let prefix = if parent_rel.is_empty() {
            String::new()
        } else {
            format!("{parent_rel}/")
        };
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ENTRIES_TABLE)?;
        let mut children = Vec::new();
        for item in table.iter()? {
            let (k, v) = item?;
            let key = k.value();
            if let Some(rest) = key.strip_prefix(&prefix)
                && !rest.is_empty()
                && !rest.contains('/')
            {
                let record = serde_json::from_slice::<PersistedEntry>(v.value())
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                children.push(record.into_entry());
            }
        }
        Ok(children)
    }

    /// Upsert an entry, keeping its existing `path_to_inode` binding if present
    /// or allocating a new constant inode if not. Returns the stored entry with
    /// its final inode.
    pub fn upsert_entry(&self, entry: &FsEntry) -> Result<FsEntry, redb::Error> {
        let rel = normalize_rel_path(&entry.path);
        let write_txn = self.db.begin_write()?;
        let mut stored = entry.clone();
        {
            let mut entries = write_txn.open_table(ENTRIES_TABLE)?;
            let mut p2i = write_txn.open_table(PATH_TO_INODE_TABLE)?;
            let mut i2p = write_txn.open_table(INODE_TO_PATH_TABLE)?;
            let mut meta = write_txn.open_table(META_TABLE)?;

            let mut next_inode = meta
                .get(NEXT_INODE_KEY)?
                .map(|v| v.value())
                .unwrap_or(2)
                .max(2);
            let mut existing_inode = p2i.get(rel.as_str())?.map(|v| v.value());
            // A changed path with the same owner and GUID is a rename, not a
            // second path binding to the same inode. Directories are handled
            // explicitly by rename_subtree because they can be merged.
            if existing_inode.is_none()
                && entry.kind != FsEntryKind::Directory
                && !entry.guid.is_empty()
            {
                let mut previous = None;
                for item in entries.iter()? {
                    let (key, value) = item?;
                    let record: PersistedEntry = serde_json::from_slice(value.value())
                        .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                    if record.node_id == entry.node_id
                        && record.guid == entry.guid
                        && record.kind != PersistedKind::Directory
                    {
                        previous = Some((key.value().to_owned(), record.inode));
                        break;
                    }
                }
                if let Some((old, inode)) = previous {
                    entries.remove(old.as_str())?;
                    p2i.remove(old.as_str())?;
                    existing_inode = Some(inode);
                }
            }
            stored.inode = if rel.is_empty() {
                1
            } else {
                existing_inode.unwrap_or_else(|| {
                    let id = next_inode;
                    next_inode += 1;
                    id
                })
            };
            next_inode = next_inode.max(stored.inode + 1).max(2);

            let mut dir_owners = Vec::new();
            let existing_bytes = entries.get(rel.as_str())?.map(|g| g.value().to_vec());
            if let Some(bytes) = existing_bytes {
                let prev: PersistedEntry = serde_json::from_slice(&bytes)
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                if prev.kind == PersistedKind::Directory && entry.kind == FsEntryKind::Directory {
                    dir_owners = prev.dir_owners;
                    if !prev.node_id.is_empty() && !dir_owners.contains(&prev.node_id) {
                        dir_owners.push(prev.node_id);
                    }
                }
            }
            if entry.kind == FsEntryKind::Directory
                && !entry.node_id.is_empty()
                && !dir_owners.contains(&entry.node_id)
            {
                dir_owners.push(entry.node_id.clone());
            }

            let encoded = serde_json::to_vec(&PersistedEntry::from_entry(&stored, dir_owners))
                .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;

            entries.insert(rel.as_str(), encoded.as_slice())?;
            p2i.insert(rel.as_str(), stored.inode)?;
            i2p.insert(stored.inode, rel.as_str())?;
            meta.insert(NEXT_INODE_KEY, next_inode)?;
        }
        write_txn.commit()?;
        Ok(stored)
    }

    pub fn remove_subtree(&self, path: &str) -> Result<usize, redb::Error> {
        let rel = normalize_rel_path(path);
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
        let write_txn = self.db.begin_write()?;
        let mut removed = 0usize;
        {
            let mut entries = write_txn.open_table(ENTRIES_TABLE)?;
            let mut p2i = write_txn.open_table(PATH_TO_INODE_TABLE)?;
            let mut i2p = write_txn.open_table(INODE_TO_PATH_TABLE)?;
            let mut targets = Vec::new();
            for item in entries.iter()? {
                let (key, _) = item?;
                if key.value() == rel || (!prefix.is_empty() && key.value().starts_with(&prefix)) {
                    targets.push(key.value().to_owned());
                }
            }
            for key in targets {
                if let Some(inode_guard) = p2i.remove(key.as_str())? {
                    i2p.remove(inode_guard.value())?;
                }
                if entries.remove(key.as_str())?.is_some() {
                    removed += 1;
                }
            }
        }
        write_txn.commit()?;
        Ok(removed)
    }

    /// Record an additional contributor to a merged directory without changing
    /// its selected owner or inode.
    pub fn add_directory_owner(&self, path: &str, owner: &str) -> Result<(), redb::Error> {
        let rel = normalize_rel_path(path);
        let txn = self.db.begin_write()?;
        {
            let mut entries = txn.open_table(ENTRIES_TABLE)?;
            let bytes = entries.get(rel.as_str())?.map(|v| v.value().to_vec());
            if let Some(bytes) = bytes {
                let mut record: PersistedEntry = serde_json::from_slice(&bytes)
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                if record.kind == PersistedKind::Directory
                    && !record.dir_owners.iter().any(|id| id == owner)
                {
                    record.dir_owners.push(owner.to_owned());
                    let value = serde_json::to_vec(&record)
                        .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                    entries.insert(rel.as_str(), value.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Remove one owner's contribution without deleting other owners' children.
    pub fn remove_owner_entry(&self, path: &str, owner: &str) -> Result<(), redb::Error> {
        let rel = normalize_rel_path(path);
        if rel.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut entries = txn.open_table(ENTRIES_TABLE)?;
            let mut p2i = txn.open_table(PATH_TO_INODE_TABLE)?;
            let mut i2p = txn.open_table(INODE_TO_PATH_TABLE)?;
            let bytes = entries.get(rel.as_str())?.map(|v| v.value().to_vec());
            if let Some(bytes) = bytes {
                let mut record: PersistedEntry = serde_json::from_slice(&bytes)
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                if record.kind == PersistedKind::Directory {
                    record.dir_owners.retain(|id| id != owner);
                    if record.node_id == owner {
                        if let Some(next) = record.dir_owners.first() {
                            record.node_id = next.clone();
                        } else {
                            entries.remove(rel.as_str())?;
                            p2i.remove(rel.as_str())?;
                            i2p.remove(record.inode)?;
                        }
                    }
                    if record.node_id != owner {
                        let value = serde_json::to_vec(&record)
                            .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                        entries.insert(rel.as_str(), value.as_slice())?;
                    }
                } else if record.node_id == owner {
                    entries.remove(rel.as_str())?;
                    p2i.remove(rel.as_str())?;
                    i2p.remove(record.inode)?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Rename `old` (and all descendants) to `new`, preserving their constant
    /// local inodes. If `new` previously existed, it is replaced and its old
    /// inode is retired.
    pub fn rename_subtree(&self, old: &str, new: &str) -> Result<Vec<FsEntry>, redb::Error> {
        let old_rel = normalize_rel_path(old);
        let new_rel = normalize_rel_path(new);
        let old_prefix = format!("{old_rel}/");
        let new_prefix = format!("{new_rel}/");

        let write_txn = self.db.begin_write()?;
        let mut moved = Vec::new();
        {
            let mut entries = write_txn.open_table(ENTRIES_TABLE)?;
            let mut p2i = write_txn.open_table(PATH_TO_INODE_TABLE)?;
            let mut i2p = write_txn.open_table(INODE_TO_PATH_TABLE)?;

            if entries.get(old_rel.as_str())?.is_none() {
                return Err(redb::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "rename source missing from catalog",
                ))
                .into());
            }
            if old_rel != new_rel {
                let mut replace_targets = Vec::new();
                for item in entries.iter()? {
                    let (key, _) = item?;
                    if key.value() == new_rel || key.value().starts_with(&new_prefix) {
                        replace_targets.push(key.value().to_owned());
                    }
                }
                for key in replace_targets {
                    if let Some(inode_guard) = p2i.remove(key.as_str())? {
                        i2p.remove(inode_guard.value())?;
                    }
                    entries.remove(key.as_str())?;
                }
            }

            let mut source_keys = Vec::new();
            for item in entries.iter()? {
                let (key, value) = item?;
                if key.value() == old_rel || key.value().starts_with(&old_prefix) {
                    source_keys.push((key.value().to_owned(), value.value().to_vec()));
                }
            }

            for (old_key, bytes) in source_keys {
                entries.remove(old_key.as_str())?;
                if let Some(inode_guard) = p2i.remove(old_key.as_str())? {
                    i2p.remove(inode_guard.value())?;
                }
                let new_key = if old_key == old_rel {
                    new_rel.clone()
                } else {
                    let rest = old_key.strip_prefix(&old_prefix).unwrap_or("");
                    format!("{new_prefix}{rest}")
                };
                let mut record: PersistedEntry = serde_json::from_slice(&bytes)
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                record.rel_path = new_key.clone();
                let encoded = serde_json::to_vec(&record)
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                entries.insert(new_key.as_str(), encoded.as_slice())?;
                p2i.insert(new_key.as_str(), record.inode)?;
                i2p.insert(record.inode, new_key.as_str())?;
                moved.push(record.into_entry());
            }
        }
        write_txn.commit()?;
        Ok(moved)
    }

    pub fn exchange_subtrees(&self, old: &str, new: &str) -> Result<Vec<FsEntry>, redb::Error> {
        let old_rel = normalize_rel_path(old);
        let new_rel = normalize_rel_path(new);
        let old_prefix = format!("{old_rel}/");
        let new_prefix = format!("{new_rel}/");

        let write_txn = self.db.begin_write()?;
        let mut updated = Vec::new();
        {
            let mut entries = write_txn.open_table(ENTRIES_TABLE)?;
            let mut p2i = write_txn.open_table(PATH_TO_INODE_TABLE)?;
            let mut i2p = write_txn.open_table(INODE_TO_PATH_TABLE)?;

            let mut affected = Vec::new();
            for item in entries.iter()? {
                let (key, value) = item?;
                if key.value() == old_rel
                    || key.value() == new_rel
                    || key.value().starts_with(&old_prefix)
                    || key.value().starts_with(&new_prefix)
                {
                    affected.push((key.value().to_owned(), value.value().to_vec()));
                }
            }

            for (key, _) in &affected {
                entries.remove(key.as_str())?;
                if let Some(inode_guard) = p2i.remove(key.as_str())? {
                    i2p.remove(inode_guard.value())?;
                }
            }

            for (key, bytes) in affected {
                let swapped = if key == old_rel {
                    new_rel.clone()
                } else if key == new_rel {
                    old_rel.clone()
                } else if let Some(rest) = key.strip_prefix(&old_prefix) {
                    format!("{new_prefix}{rest}")
                } else if let Some(rest) = key.strip_prefix(&new_prefix) {
                    format!("{old_prefix}{rest}")
                } else {
                    key
                };
                let mut record: PersistedEntry = serde_json::from_slice(&bytes)
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                record.rel_path = swapped.clone();
                let encoded = serde_json::to_vec(&record)
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                entries.insert(swapped.as_str(), encoded.as_slice())?;
                p2i.insert(swapped.as_str(), record.inode)?;
                i2p.insert(record.inode, swapped.as_str())?;
                updated.push(record.into_entry());
            }
        }
        write_txn.commit()?;
        Ok(updated)
    }

    /// Reconcile a full owner snapshot into `redb` (used at startup local scan
    /// and on peer handshake). Removes any non-directory entry owned by
    /// `owner_id` that is not in `present_paths`, and upserts all provided
    /// entries while preserving constant inodes.
    pub fn reconcile_owner_snapshot(
        &self,
        owner_id: &str,
        snapshot: &[FsEntry],
    ) -> Result<(Vec<FsEntry>, Vec<String>), redb::Error> {
        let mut present_rel: HashSet<String> = HashSet::new();
        for entry in snapshot {
            present_rel.insert(normalize_rel_path(&entry.path));
        }

        let write_txn = self.db.begin_write()?;
        let mut stored_entries = Vec::with_capacity(snapshot.len());
        let mut conflicts = Vec::new();
        {
            let mut entries = write_txn.open_table(ENTRIES_TABLE)?;
            let mut p2i = write_txn.open_table(PATH_TO_INODE_TABLE)?;
            let mut i2p = write_txn.open_table(INODE_TO_PATH_TABLE)?;
            let mut meta = write_txn.open_table(META_TABLE)?;
            let mut next_inode = meta
                .get(NEXT_INODE_KEY)?
                .map(|v| v.value())
                .unwrap_or(2)
                .max(2);

            // Record identities before deleting stale paths so a missed rename
            // can retain its local inode after a restart or reconnect.
            let mut renamed_inodes = HashMap::new();
            let mut stale = Vec::new();
            for item in entries.iter()? {
                let (key, value) = item?;
                if key.value().is_empty() || present_rel.contains(key.value()) {
                    continue;
                }
                let record: PersistedEntry = serde_json::from_slice(value.value())
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                if record.node_id == owner_id || record.dir_owners.iter().any(|id| id == owner_id) {
                    if record.node_id == owner_id
                        && record.kind != PersistedKind::Directory
                        && !record.guid.is_empty()
                    {
                        renamed_inodes.insert(record.guid.clone(), record.inode);
                    }
                    stale.push((key.value().to_owned(), record));
                }
            }
            for (key, mut record) in stale {
                if record.kind == PersistedKind::Directory {
                    record.dir_owners.retain(|id| id != owner_id);
                    if record.node_id == owner_id {
                        if let Some(next) = record.dir_owners.first() {
                            record.node_id = next.clone();
                        } else {
                            entries.remove(key.as_str())?;
                            p2i.remove(key.as_str())?;
                            i2p.remove(record.inode)?;
                            continue;
                        }
                    }
                    let encoded = serde_json::to_vec(&record)
                        .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                    entries.insert(key.as_str(), encoded.as_slice())?;
                } else if record.node_id == owner_id {
                    p2i.remove(key.as_str())?;
                    i2p.remove(record.inode)?;
                    entries.remove(key.as_str())?;
                }
            }

            for entry in snapshot {
                let rel = normalize_rel_path(&entry.path);
                let existing_bytes = entries.get(rel.as_str())?.map(|g| g.value().to_vec());
                let mut dir_owners = Vec::new();
                let mut replaced_inode = None;
                if let Some(bytes) = existing_bytes {
                    let prev: PersistedEntry = serde_json::from_slice(&bytes)
                        .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                    if prev.node_id != owner_id {
                        if prev.kind == PersistedKind::Directory
                            && entry.kind == FsEntryKind::Directory
                        {
                            dir_owners = prev.dir_owners;
                            if !prev.node_id.is_empty() && !dir_owners.contains(&prev.node_id) {
                                dir_owners.push(prev.node_id);
                            }
                            if !dir_owners.contains(&owner_id.to_owned()) {
                                dir_owners.push(owner_id.to_owned());
                            }
                        } else {
                            // Rule #2: two nodes cannot own the same file path.
                            conflicts.push(to_logical_path(&rel));
                            continue;
                        }
                    } else if entry.kind != FsEntryKind::Directory
                        && prev.kind != PersistedKind::Directory
                        && !prev.guid.is_empty()
                        && !entry.guid.is_empty()
                        && prev.guid != entry.guid
                    {
                        // A different stable identity at the same path is a
                        // replacement, not the old inode being resurrected.
                        replaced_inode = Some(prev.inode);
                    } else if entry.kind == FsEntryKind::Directory {
                        dir_owners = prev.dir_owners;
                        if !dir_owners.contains(&owner_id.to_owned()) {
                            dir_owners.push(owner_id.to_owned());
                        }
                    }
                } else if entry.kind == FsEntryKind::Directory && !owner_id.is_empty() {
                    dir_owners.push(owner_id.to_owned());
                }

                if let Some(inode) = replaced_inode {
                    p2i.remove(rel.as_str())?;
                    i2p.remove(inode)?;
                }
                let existing_inode = p2i.get(rel.as_str())?.map(|v| v.value());
                let mut stored = entry.clone();
                stored.inode = if rel.is_empty() {
                    1
                } else {
                    existing_inode.unwrap_or_else(|| {
                        renamed_inodes.remove(&entry.guid).unwrap_or_else(|| {
                            let id = next_inode;
                            next_inode += 1;
                            id
                        })
                    })
                };
                next_inode = next_inode.max(stored.inode + 1).max(2);

                let encoded = serde_json::to_vec(&PersistedEntry::from_entry(&stored, dir_owners))
                    .map_err(|e| redb::StorageError::Io(std::io::Error::other(e)))?;
                entries.insert(rel.as_str(), encoded.as_slice())?;
                p2i.insert(rel.as_str(), stored.inode)?;
                i2p.insert(stored.inode, rel.as_str())?;
                stored_entries.push(stored);
            }

            meta.insert(NEXT_INODE_KEY, next_inode)?;
        }
        write_txn.commit()?;
        Ok((stored_entries, conflicts))
    }
}

#[cfg(test)]
mod queued_catalog_tests {
    // The redb write transaction is private. Holding it open is how this
    // test proves that foreground operations finish before a commit, without
    // exposing a production pause hook or relying on scheduling and sleeps.
    use super::*;
    use crate::core::cache::metadata::MetadataCache;

    fn file(path: &str, guid: &str, size: u64) -> FsEntry {
        FsEntry {
            inode: 0,
            path: path.into(),
            guid: guid.into(),
            kind: FsEntryKind::File,
            node_id: "owner".into(),
            availability: Availability::Available,
            attr: FileAttrData {
                guid: guid.into(),
                kind: FsEntryKind::File,
                stat: Stat {
                    size,
                    ..Stat::default()
                },
                ..FileAttrData::default()
            },
        }
    }

    #[test]
    fn corrupt_catalog_row_is_not_a_cache_miss() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(PersistentDb::open(&dir.path().join("catalog.redb")).unwrap());
        let mut cache = MetadataCache::new();
        cache.attach_db(db.clone(), None, Some(1)).unwrap();
        let txn = db.db.begin_write().unwrap();
        txn.open_table(ENTRIES_TABLE)
            .unwrap()
            .insert("broken", b"bad row".as_slice())
            .unwrap();
        txn.commit().unwrap();
        assert_eq!(cache.try_lookup_by_path(None, "/absent").unwrap(), None);
        assert!(cache.try_lookup_by_path(None, "/broken").is_err());
        assert!(cache.try_lookup_children(None, "/").is_err());
    }

    #[test]
    fn pending_changes_override_redb_until_ordered_commit() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.redb");
        let db = Arc::new(PersistentDb::open(&path).unwrap());
        let removed = db.upsert_entry(&file("/removed", "old-guid", 1)).unwrap();
        let updated = db
            .upsert_entry(&file("/updated", "update-guid", 1))
            .unwrap();
        db.upsert_entry(&file("/cold", "cold-guid", 1)).unwrap();
        let mut cache = MetadataCache::new();
        cache
            .attach_db(db.clone(), Some(Duration::ZERO), Some(1))
            .unwrap();
        let hold = db.db.begin_write().unwrap();
        cache.insert_root("/", "/", FsEntryKind::Directory);
        cache.upsert_entry(file("/updated", "update-guid", 12));
        cache.upsert_entry(file("/pending", "new-guid", 6));
        cache.remove_entry("/removed");
        assert_eq!(
            db.get_by_path("/updated").unwrap().unwrap().attr.stat.size,
            1
        );
        assert!(db.get_by_path("/pending").unwrap().is_none());
        assert!(db.get_by_path("/removed").unwrap().is_some());
        assert_eq!(cache.lookup_inode("/removed"), None);
        assert_eq!(cache.lookup_path(removed.inode), None);
        assert!(cache.lookup_entry_by_guid("owner", "old-guid").is_none());
        assert!(cache.entries.len() <= 1);
        assert_eq!(
            cache
                .lookup_children_of("/")
                .into_iter()
                .map(|e| (e.path, e.attr.stat.size))
                .collect::<Vec<_>>(),
            vec![
                ("/cold".into(), 1),
                ("/pending".into(), 6),
                ("/updated".into(), 12)
            ]
        );

        cache.upsert_entry(file("/removed", "old-guid", 4));
        let recreated = cache.lookup_inode("/removed").unwrap();
        assert_ne!(recreated, removed.inode);
        assert!(cache.rename_entry("/pending", "/renamed"));
        let renamed = cache.lookup_inode("/renamed").unwrap();
        assert_eq!(cache.lookup_inode("/pending"), None);
        assert_eq!(cache.lookup_path(renamed).as_deref(), Some("/renamed"));
        drop(hold);
        drop(cache); // clean shutdown drains the ordered queue
        drop(db);

        let db = PersistentDb::open(&path).unwrap();
        assert_eq!(db.get_inode("/removed").unwrap(), Some(recreated));
        assert_eq!(db.get_path(removed.inode).unwrap(), None);
        assert_eq!(db.get_inode("/renamed").unwrap(), Some(renamed));
        assert_eq!(db.get_inode("/pending").unwrap(), None);
        assert_eq!(
            db.get_by_inode(updated.inode)
                .unwrap()
                .unwrap()
                .attr
                .stat
                .size,
            12
        );
    }
}
