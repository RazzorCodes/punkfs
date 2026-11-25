//! The client-side catalog: inodes, paths, and last-known metadata.
//!
//! Two distinct identities are in play:
//!
//! - **`guid`** is the punkfs identity of an entry. It is what the catalog
//!   (and later the coordinator) uses to say "this is the same file", so it
//!   survives renames, re-listings, and a client restart.
//! - **`inode`** is a purely local FUSE convenience: the kernel needs a
//!   `u64` and we need something cheap to index entries by. It is allocated
//!   by this client and means nothing outside it.
//!
//! Entries are therefore looked up by `guid` when we reconcile a listing,
//! and by `inode`/`path` when the kernel asks about something.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::core::types::{Availability, FsEntry};
use crate::core::types::{FileAttrData, FsEntryKind};
use crate::db::persistent::{
    CatalogPatch, CatalogRow, PersistentDb, normalize_rel_path, to_logical_path,
};
use crate::db::worker::CatalogWriter;
use crate::telemetry::{self, Event, Logger, OperationContext, Severity};

pub struct FsNode {
    pub node_id: String,
}

pub struct FsNodeMetadata {
    pub parent_node: String,
    pub guid: String,
}

/// One node in the cluster, as the catalog remembers it.
///
/// `last_seen` is the pull-model liveness record: it is stamped on every
/// successful rpc to that node and is the single source of truth for
/// [`Availability`]. `None` means the node has never answered.
#[derive(Clone, Debug)]
pub struct NodeStatus {
    pub node_id: String,
    pub last_seen: Option<SystemTime>,
}

impl NodeStatus {
    pub fn new(node_id: impl Into<String>) -> Self {
        NodeStatus {
            node_id: node_id.into(),
            last_seen: None,
        }
    }

    /// Record that the node just answered.
    pub fn mark_seen(&mut self, at: SystemTime) {
        self.last_seen = Some(at);
    }

    /// Whether the node answered within `max_age` of `now`.
    ///
    /// A node that has never answered is unreachable, not reachable-by-
    /// default: assuming health would make a fresh client silently serve
    /// entries it cannot actually read.
    pub fn is_reachable(&self, now: SystemTime, max_age: Duration) -> bool {
        self.last_seen
            .and_then(|seen| now.duration_since(seen).ok())
            .is_some_and(|age| age <= max_age)
    }
}

/// One node's answer to a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingOutcome {
    /// The node answered; `children` is authoritative for its own entries.
    Answered(Vec<FsEntry>),
    /// The node could not be reached; its cached entries stay.
    Unreachable,
}

pub struct MetadataCache {
    pub entries: Vec<FsEntry>,
    next_inode: u64,
    logger: Option<Logger>,
    local_node: Option<String>,
    db: Option<Arc<PersistentDb>>,
    writer: Option<CatalogWriter>,
    // Separate from the hot cache: an unacknowledged row cannot be evicted.
    // A tombstone masks committed DB data by logical path, including reverse
    // inode and GUID lookups.
    pending: BTreeMap<String, (u64, Option<CatalogRow>)>,
    memory_directory_owners: HashMap<String, Vec<String>>,
    ttl: Option<Duration>,
    max_entries: Option<usize>,
    cached_at: HashMap<String, Instant>,
    last_used: HashMap<String, Instant>,
    persistence_error: Option<String>,
}

impl Default for MetadataCache {
    fn default() -> Self {
        MetadataCache::new()
    }
}

impl MetadataCache {
    /// Use the process logger when one has been initialized.
    pub fn new() -> Self {
        Self::create(None)
    }

    /// Use an explicitly supplied logger instead of the process logger.
    pub fn with_logger(logger: Logger) -> Self {
        Self::create(Some(logger))
    }

    fn create(logger: Option<Logger>) -> Self {
        let cache = MetadataCache {
            entries: Vec::new(),
            next_inode: 1,
            logger,
            local_node: None,
            db: None,
            writer: None,
            pending: BTreeMap::new(),
            memory_directory_owners: HashMap::new(),
            ttl: None,
            max_entries: None,
            cached_at: HashMap::new(),
            last_used: HashMap::new(),
            persistence_error: None,
        };
        cache.emit(
            None,
            Severity::Info,
            "initialize",
            "metadata cache initialized",
            &[],
        );
        cache
    }

    /// Attach a `redb` persistence backend and bounded in-memory cache policy.
    /// Existing entries are hydrated up to `max_entries`. Once attached, this
    /// cache's worker must be the only writer to the database.
    pub fn attach_db(
        &mut self,
        db: Arc<PersistentDb>,
        ttl: Option<Duration>,
        max_entries: Option<usize>,
    ) -> Result<(), redb::Error> {
        if self.writer.is_some() {
            return Err(
                redb::StorageError::Io(std::io::Error::other("catalog already attached")).into(),
            );
        }
        let (loaded, next_inode) = db.load_limit(max_entries)?;
        self.writer = Some(
            CatalogWriter::start(db.clone(), self.logger.clone())
                .map_err(redb::StorageError::Io)?,
        );
        self.next_inode = self.next_inode.max(next_inode);
        self.db = Some(db);
        self.ttl = ttl;
        self.max_entries = max_entries;
        let now = Instant::now();
        for entry in loaded {
            if self.entries.iter().all(|e| e.path != entry.path) {
                self.cached_at.insert(entry.path.clone(), now);
                self.last_used.insert(entry.path.clone(), now);
                self.entries.push(entry);
            }
        }
        self.evict_excess();
        Ok(())
    }

    /// A failed catalog write makes the process's view unreliable. Keep the
    /// error until restart rather than reporting later operations as durable.
    pub(crate) fn persistence_error(&self) -> Option<String> {
        self.persistence_error
            .clone()
            .or_else(|| self.writer.as_ref().and_then(CatalogWriter::failure))
    }

    /// Synchronous read/admission failure, distinct from an asynchronously
    /// failed commit. Do not roll back a backing operation after its update
    /// has already been accepted into the queue.
    pub(crate) fn admission_error(&self) -> Option<&str> {
        self.persistence_error.as_deref()
    }

    /// Wait for already accepted updates to reach the durable catalog. This
    /// is a lifecycle barrier, not part of normal foreground mutations.
    pub fn flush(&mut self) -> Result<(), String> {
        if let Some(writer) = &self.writer {
            writer.flush()?;
        }
        self.reap_committed();
        self.persistence_error().map_or(Ok(()), Err)
    }

    pub(crate) fn reconcile_persistent_snapshot(
        &mut self,
        ctx: &OperationContext,
        owner: &str,
        snapshot: Vec<FsEntry>,
    ) -> Result<(usize, Vec<String>), String> {
        let writer = self.writer.as_ref().expect("persistent catalog");
        let (stored, conflicts) = writer.reconcile_snapshot(ctx, owner, snapshot)?;
        // The snapshot ran after all queued changes and may allocate inodes.
        self.reap_committed();
        self.entries.clear();
        self.cached_at.clear();
        self.last_used.clear();
        let next = self
            .db
            .as_ref()
            .unwrap()
            .next_inode()
            .map_err(|error| self.read_error(Some(ctx), "snapshot_next_inode", error))?;
        self.next_inode = self.next_inode.max(next);
        Ok((stored.len(), conflicts))
    }

    fn reap_committed(&mut self) {
        if let Some(writer) = &self.writer {
            let committed = writer.committed();
            self.pending.retain(|_, (seq, _)| *seq > committed);
        }
    }

    fn record_db_error(
        &mut self,
        ctx: Option<&OperationContext>,
        operation: &str,
        error: impl std::fmt::Display,
    ) {
        let message = error.to_string();
        self.emit(
            ctx,
            Severity::Error,
            operation,
            "persistent metadata operation failed",
            &[("error", serde_json::json!(&message))],
        );
        if self.persistence_error.is_none() {
            self.persistence_error = Some(message);
        }
    }

    pub(crate) fn is_persistent(&self) -> bool {
        self.db.is_some()
    }

    /// Drop expired or over-capacity entries from the *in-memory* cache.
    /// Entries remain in `redb` so `get_inode`/`get_path` still resolve
    /// constant local inodes on cache misses.
    pub fn prune_expired(&mut self) {
        self.reap_committed();
        if let Some(ttl) = self.ttl {
            let now = Instant::now();
            let expired: HashSet<String> = self
                .entries
                .iter()
                .filter(|e| e.path != "/")
                .filter(|e| {
                    self.cached_at
                        .get(&e.path)
                        .is_some_and(|at| now.saturating_duration_since(*at) > ttl)
                })
                .map(|e| e.path.clone())
                .collect();
            if !expired.is_empty() {
                self.entries.retain(|e| !expired.contains(&e.path));
                for path in expired {
                    self.cached_at.remove(&path);
                    self.last_used.remove(&path);
                }
            }
        }
        self.evict_excess();
    }

    fn evict_excess(&mut self) {
        let Some(max) = self.max_entries else {
            return;
        };
        let excess = self.entries.len().saturating_sub(max);
        if excess == 0 {
            return;
        }
        let mut candidates: Vec<_> = self.entries.iter().filter(|e| e.path != "/").collect();
        if excess < candidates.len() {
            candidates.select_nth_unstable_by_key(excess, |e| self.last_used.get(&e.path).copied());
            candidates.truncate(excess);
        }
        let evicted: HashSet<_> = candidates.into_iter().map(|e| e.path.clone()).collect();
        self.entries.retain(|e| !evicted.contains(&e.path));
        for path in evicted {
            self.cached_at.remove(&path);
            self.last_used.remove(&path);
        }
    }

    fn touch_mem(&mut self, path: &str) {
        let now = Instant::now();
        self.cached_at.entry(path.to_owned()).or_insert(now);
        self.last_used.insert(path.to_owned(), now);
    }

    fn current_row(
        &mut self,
        ctx: Option<&OperationContext>,
        path: &str,
    ) -> Result<Option<CatalogRow>, String> {
        if let Some((_, row)) = self.pending.get(path) {
            return Ok(row.clone());
        }
        // Only directories need owner data in addition to the hot entry.
        if let Some(entry) = self
            .get_entry_by_path(path)
            .filter(|entry| entry.kind != FsEntryKind::Directory)
        {
            return Ok(Some(CatalogRow {
                entry: entry.clone(),
                directory_owners: Vec::new(),
            }));
        }
        let mut row = if let Some(db) = &self.db {
            db.get_row(path)
                .map_err(|error| self.read_error(ctx, "lookup_path", error))?
        } else {
            None
        };
        if let Some(resident) = self.get_entry_by_path(path) {
            row = Some(CatalogRow {
                entry: resident.clone(),
                directory_owners: row.map(|r| r.directory_owners).unwrap_or_else(|| {
                    self.memory_directory_owners
                        .get(path)
                        .cloned()
                        .unwrap_or_default()
                }),
            });
        }
        Ok(row)
    }

    fn read_error(
        &mut self,
        ctx: Option<&OperationContext>,
        operation: &str,
        err: redb::Error,
    ) -> String {
        self.record_db_error(ctx, operation, &err);
        err.to_string()
    }

    pub fn lookup_entry_by_path(&mut self, path: &str) -> Option<FsEntry> {
        self.try_lookup_by_path(None, path).ok().flatten()
    }

    /// Cache-first lookup, with DB I/O errors distinguishable from a miss.
    pub(crate) fn try_lookup_by_path(
        &mut self,
        ctx: Option<&OperationContext>,
        path: &str,
    ) -> Result<Option<FsEntry>, String> {
        let path = to_logical_path(&normalize_rel_path(path));
        self.prune_expired();
        if let Some((_, row)) = self.pending.get(&path) {
            return Ok(row.as_ref().map(|row| row.entry.clone()));
        }
        if let Some(entry) = self.get_entry_by_path(&path).cloned() {
            self.touch_mem(&path);
            return Ok(Some(entry));
        }
        let Some(db) = &self.db else {
            return Ok(None);
        };
        let entry = db
            .get_by_path(&path)
            .map_err(|err| self.read_error(ctx, "lookup_path", err))?;
        self.warm(entry.as_ref());
        Ok(entry)
    }

    /// Find a stable identity regardless of in-memory eviction.
    pub fn lookup_entry_by_guid(&mut self, owner: &str, guid: &str) -> Option<FsEntry> {
        self.prune_expired();
        self.try_lookup_guid(None, owner, guid).ok().flatten()
    }

    fn try_lookup_guid(
        &mut self,
        ctx: Option<&OperationContext>,
        owner: &str,
        guid: &str,
    ) -> Result<Option<FsEntry>, String> {
        if guid.is_empty() {
            return Ok(None);
        }
        if let Some(entry) = self
            .pending
            .values()
            .filter_map(|(_, row)| row.as_ref().map(|r| &r.entry))
            .chain(self.entries.iter())
            .find(|entry| entry.node_id == owner && entry.guid == guid)
            .cloned()
        {
            if self.get_entry_by_path(&entry.path).is_some() {
                self.touch_mem(&entry.path);
            }
            return Ok(Some(entry));
        }
        let Some(db) = &self.db else {
            return Ok(None);
        };
        let entry = db
            .get_by_guid(owner, guid)
            .map_err(|err| self.read_error(ctx, "lookup_guid", err))?;
        Ok(entry.filter(|entry| !self.pending.contains_key(&entry.path)))
    }

    pub fn lookup_inode(&mut self, path: &str) -> Option<u64> {
        self.lookup_entry_by_path(path).map(|entry| entry.inode)
    }

    pub fn lookup_path(&mut self, inode: u64) -> Option<String> {
        self.lookup_entry_by_inode(inode).map(|entry| entry.path)
    }

    pub fn lookup_entry_by_inode(&mut self, inode: u64) -> Option<FsEntry> {
        self.try_lookup_by_inode(None, inode).ok().flatten()
    }

    pub(crate) fn try_lookup_by_inode(
        &mut self,
        ctx: Option<&OperationContext>,
        inode: u64,
    ) -> Result<Option<FsEntry>, String> {
        self.prune_expired();
        if let Some(entry) = self
            .pending
            .values()
            .filter_map(|(_, row)| row.as_ref().map(|r| &r.entry))
            .chain(self.entries.iter())
            .find(|e| e.inode == inode)
            .cloned()
        {
            if self.get_entry_by_path(&entry.path).is_some() {
                self.touch_mem(&entry.path);
            }
            return Ok(Some(entry));
        }
        let Some(db) = &self.db else {
            return Ok(None);
        };
        let entry = db
            .get_by_inode(inode)
            .map_err(|err| self.read_error(ctx, "lookup_inode", err))?;
        let entry = entry.filter(|e| !self.pending.contains_key(&e.path));
        self.warm(entry.as_ref());
        Ok(entry)
    }

    fn warm(&mut self, entry: Option<&FsEntry>) {
        if let Some(entry) = entry {
            self.touch_mem(&entry.path);
            self.entries.push(entry.clone());
            self.evict_excess();
        }
    }

    /// Union direct children by path. Resident and queued rows take priority
    /// over redb; a queued deletion suppresses the durable row entirely.
    pub fn lookup_children_of(&mut self, path: &str) -> Vec<FsEntry> {
        self.try_lookup_children(None, path).unwrap_or_default()
    }

    pub(crate) fn try_lookup_children(
        &mut self,
        ctx: Option<&OperationContext>,
        path: &str,
    ) -> Result<Vec<FsEntry>, String> {
        self.prune_expired();
        Ok(self
            .rows_under(ctx, path, true)?
            .into_values()
            .map(|r| r.entry)
            .collect())
    }

    fn rows_under(
        &mut self,
        ctx: Option<&OperationContext>,
        path: &str,
        children_only: bool,
    ) -> Result<BTreeMap<String, CatalogRow>, String> {
        let path = to_logical_path(&normalize_rel_path(path));
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let matches = |key: &str| {
            (!children_only && key == path)
                || key.strip_prefix(&prefix).is_some_and(|suffix| {
                    !suffix.is_empty() && (!children_only || !suffix.contains('/'))
                })
        };
        let mut rows: BTreeMap<String, CatalogRow> = if let Some(db) = &self.db {
            db.rows_under(&path, children_only)
                .map_err(|err| self.read_error(ctx, "lookup_children", err))?
                .into_iter()
                .map(|row| (row.entry.path.clone(), row))
                .collect()
        } else {
            BTreeMap::new()
        };
        for entry in self.entries.iter().filter(|e| matches(&e.path)) {
            if let Some(row) = rows.get_mut(&entry.path) {
                row.entry = entry.clone();
            } else {
                rows.insert(
                    entry.path.clone(),
                    CatalogRow {
                        entry: entry.clone(),
                        directory_owners: self
                            .memory_directory_owners
                            .get(&entry.path)
                            .cloned()
                            .unwrap_or_default(),
                    },
                );
            }
        }
        for (path, (_, row)) in self.pending.iter().filter(|(p, _)| matches(p)) {
            if let Some(row) = row {
                rows.insert(path.clone(), row.clone());
            } else {
                rows.remove(path);
            }
        }
        Ok(rows)
    }

    fn apply(
        &mut self,
        ctx: &OperationContext,
        operation: &'static str,
        patch: CatalogPatch,
    ) -> bool {
        if patch.is_empty() {
            return true;
        }
        let patch = Arc::new(patch);
        if let Some(writer) = &mut self.writer {
            match writer.submit(ctx, operation, patch.clone()) {
                Ok(seq) => {
                    self.reap_committed();
                    for (path, row) in patch.iter() {
                        self.pending.insert(path.clone(), (seq, row.clone()));
                    }
                }
                Err(error) => {
                    self.record_db_error(Some(ctx), operation, error);
                    return false;
                }
            }
        }
        self.entries.retain(|e| !patch.contains_key(&e.path));
        for (path, row) in patch.iter() {
            self.cached_at.remove(path);
            self.last_used.remove(path);
            self.memory_directory_owners.remove(path);
            if let Some(row) = row {
                self.touch_mem(path);
                self.entries.push(row.entry.clone());
                if self.db.is_none() && !row.directory_owners.is_empty() {
                    self.memory_directory_owners
                        .insert(path.clone(), row.directory_owners.clone());
                }
            }
        }
        self.evict_excess();
        true
    }

    /// Mark a node as locally accessible even during remote-only reconciliation.
    pub(crate) fn set_local_node(&mut self, node: String) {
        self.local_node = Some(node);
    }

    fn emit(
        &self,
        ctx: Option<&OperationContext>,
        severity: Severity,
        operation: &str,
        message: &str,
        fields: &[(&str, serde_json::Value)],
    ) {
        let Some(logger) = self.logger.as_ref().or_else(|| telemetry::global()) else {
            return;
        };
        if !logger.enabled(severity) {
            return;
        }
        let mut event = Event::new(severity, "metadata_cache", message);
        event.operation = Some(operation.to_owned());
        event.fields.extend(
            fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone())),
        );
        if let Some(ctx) = ctx {
            logger.emit_in(ctx, event);
        } else {
            logger.emit(event);
        }
    }

    /// Allocate a fresh local inode. Never reuses one, so a stale inode in
    /// the kernel cannot silently alias a different file.
    fn alloc_inode(&mut self) -> u64 {
        let inode = self.next_inode;
        self.next_inode += 1;
        inode
    }

    pub fn get_entry_by_path(&self, path: &str) -> Option<&FsEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    pub fn get_entry_by_guid(&self, guid: &str) -> Option<&FsEntry> {
        self.entries.iter().find(|e| e.guid == guid)
    }

    pub fn get_inode(&self, path: &str) -> Option<u64> {
        self.get_entry_by_path(path).map(|e| e.inode)
    }

    pub fn get_path(&self, inode: u64) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.inode == inode)
            .map(|e| e.path.as_str())
    }

    pub fn get_entry(&self, inode: u64) -> Option<&FsEntry> {
        self.entries.iter().find(|e| e.inode == inode)
    }

    /// Insert or update an entry.
    ///
    /// Matching order is **guid first, then path**, which is what keeps a
    /// renamed file pointing at the same local inode: the reconciler sees a
    /// new path carrying a known guid and updates the existing entry rather
    /// than allocating a second one.
    ///
    /// - `inode == 0` means "allocate a local inode".
    /// - `metadata` replaces the stored attributes wholesale.
    ///
    /// Returns the old entry if one was replaced.
    pub fn upsert_entry(&mut self, entry: FsEntry) -> Option<FsEntry> {
        self.upsert_entry_in(&OperationContext::new(), entry)
    }

    /// Insert or update an entry as part of an existing operation.
    pub fn upsert_entry_in(
        &mut self,
        ctx: &OperationContext,
        mut entry: FsEntry,
    ) -> Option<FsEntry> {
        self.prune_expired();
        entry.path = to_logical_path(&normalize_rel_path(&entry.path));
        let by_path = self.current_row(Some(ctx), &entry.path).ok()?;
        let previous = if by_path.is_none() && entry.kind != FsEntryKind::Directory {
            self.try_lookup_guid(Some(ctx), &entry.node_id, &entry.guid)
                .ok()?
                .filter(|e| e.kind != FsEntryKind::Directory)
        } else {
            None
        };
        let row = self.make_row(entry, by_path.as_ref(), previous.as_ref());
        let old = by_path
            .as_ref()
            .map(|r| r.entry.clone())
            .or_else(|| previous.clone());
        if by_path.as_ref() == Some(&row) {
            if self.get_entry_by_path(&row.entry.path).is_some() {
                self.touch_mem(&row.entry.path);
            }
            return old;
        }
        let mut patch = CatalogPatch::new();
        if let Some(previous) = previous {
            patch.insert(previous.path, None);
        }
        patch.insert(row.entry.path.clone(), Some(row));
        if !self.apply(ctx, "upsert", patch) {
            return None;
        }
        self.emit(
            Some(ctx),
            Severity::Trace,
            "upsert",
            "metadata cache entry updated",
            &[(
                "outcome",
                serde_json::json!(if old.is_some() {
                    "replaced"
                } else {
                    "inserted"
                }),
            )],
        );
        old
    }

    fn make_row(
        &mut self,
        mut entry: FsEntry,
        same_path: Option<&CatalogRow>,
        previous: Option<&FsEntry>,
    ) -> CatalogRow {
        if entry.inode == 0 {
            entry.inode = by_identity(same_path, previous).unwrap_or_else(|| self.alloc_inode());
        }
        self.next_inode = self.next_inode.max(entry.inode + 1);
        let mut directory_owners = Vec::new();
        if entry.kind == FsEntryKind::Directory {
            if let Some(row) = same_path.filter(|r| r.entry.kind == FsEntryKind::Directory) {
                directory_owners.clone_from(&row.directory_owners);
                add_owner(&mut directory_owners, &row.entry.node_id);
            }
            add_owner(&mut directory_owners, &entry.node_id);
        }
        CatalogRow {
            entry,
            directory_owners,
        }
    }

    /// Seed a root at its reserved inode.
    pub fn insert_root(&mut self, path: &str, guid: &str, kind: FsEntryKind) {
        self.upsert_entry(FsEntry {
            inode: 1,
            path: path.to_owned(),
            guid: guid.to_owned(),
            kind,
            node_id: String::new(),
            availability: Availability::Available,
            attr: FileAttrData {
                guid: guid.to_owned(),
                kind,
                ..FileAttrData::default()
            },
        });
    }

    pub fn has_children(&self, path: &str) -> bool {
        !self.get_children_of(path).is_empty()
    }

    /// Drop `path` and, for a directory, everything below it.
    pub fn remove_entry(&mut self, path: &str) -> Option<FsEntry> {
        self.remove_entry_in(&OperationContext::new(), path)
    }

    pub fn remove_entry_in(&mut self, ctx: &OperationContext, path: &str) -> Option<FsEntry> {
        self.prune_expired();
        let rows = self.rows_under(Some(ctx), path, false).ok()?;
        let removed = rows
            .get(&to_logical_path(&normalize_rel_path(path)))
            .map(|r| r.entry.clone());
        let count = rows.len();
        if !self.apply(
            ctx,
            "remove",
            rows.into_keys().map(|key| (key, None)).collect(),
        ) {
            return None;
        }
        self.emit(
            Some(ctx),
            Severity::Debug,
            "remove",
            "metadata cache removal completed",
            &[("entries_removed", serde_json::json!(count))],
        );
        removed
    }

    /// Remove only this owner's contribution to a path. Other contributors
    /// and their descendants must survive a removal of a merged directory.
    pub(crate) fn remove_owner_entry_in(
        &mut self,
        ctx: &OperationContext,
        path: &str,
        owner: &str,
    ) {
        self.prune_expired();
        let path = to_logical_path(&normalize_rel_path(path));
        if path == "/" {
            return;
        }
        if let Ok(Some(row)) = self.current_row(Some(ctx), &path) {
            let survivor = remove_owner(row.clone(), owner);
            if survivor.as_ref() != Some(&row)
                && self.apply(
                    ctx,
                    "remove",
                    CatalogPatch::from([(path.to_owned(), survivor)]),
                )
            {
                self.emit(
                    Some(ctx),
                    Severity::Debug,
                    "remove",
                    "metadata cache owner removal completed",
                    &[],
                );
            }
        }
    }

    /// Move `old` (and its subtree) to `new`, keeping inodes and GUIDs.
    pub fn rename_entry(&mut self, old: &str, new: &str) -> bool {
        self.rename_entry_in(&OperationContext::new(), old, new)
    }

    pub fn rename_entry_in(&mut self, ctx: &OperationContext, old: &str, new: &str) -> bool {
        self.move_entries(ctx, old, new, false)
    }

    /// Exchange two cached subtrees without changing their inode identities.
    pub fn exchange_entries(&mut self, old: &str, new: &str) {
        self.exchange_entries_in(&OperationContext::new(), old, new);
    }

    pub(crate) fn exchange_entries_in(&mut self, ctx: &OperationContext, old: &str, new: &str) {
        self.move_entries(ctx, old, new, true);
    }

    pub(crate) fn owner_rename_allowed(
        &mut self,
        ctx: &OperationContext,
        old: &str,
        new: &str,
        owner: &str,
    ) -> Result<bool, String> {
        self.prune_expired();
        if old == new {
            return Ok(true);
        }
        for path in [old, new] {
            if self
                .rows_under(Some(ctx), path, false)?
                .values()
                .any(|row| {
                    (!row.entry.node_id.is_empty() && row.entry.node_id != owner)
                        || row.directory_owners.iter().any(|id| id != owner)
                })
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn move_entries(
        &mut self,
        ctx: &OperationContext,
        old: &str,
        new: &str,
        exchange: bool,
    ) -> bool {
        let old = to_logical_path(&normalize_rel_path(old));
        let new = to_logical_path(&normalize_rel_path(new));
        let (old, new) = (old.as_str(), new.as_str());
        if old == "/" || new == "/" {
            return false;
        }
        self.prune_expired();
        let Ok(source) = self.rows_under(Some(ctx), old, false) else {
            return false;
        };
        if source.is_empty() {
            return false;
        }
        if old == new {
            return true;
        }
        let Ok(target) = self.rows_under(Some(ctx), new, false) else {
            return false;
        };
        let count = source.len() + if exchange { target.len() } else { 0 };
        let mut patch: CatalogPatch = source
            .keys()
            .chain(target.keys())
            .map(|p| (p.clone(), None))
            .collect();
        let mut move_rows = |rows: BTreeMap<String, CatalogRow>, from: &str, to: &str| {
            for (path, mut row) in rows {
                let next = format!("{to}{}", &path[from.len()..]);
                row.entry.path = next.clone();
                patch.insert(next, Some(row));
            }
        };
        move_rows(source, old, new);
        if exchange {
            move_rows(target, new, old);
        }
        let operation = if exchange { "exchange" } else { "rename" };
        if !self.apply(ctx, operation, patch) {
            return false;
        }
        self.emit(
            Some(ctx),
            Severity::Debug,
            operation,
            "metadata cache rename completed",
            &[("entries_renamed", serde_json::json!(count))],
        );
        true
    }

    /// Forget everything cached below `path`, keeping `path` itself.
    ///
    /// Called after a mutation so the next `readdir` refetches instead of
    /// serving entries the server has already changed.
    pub fn invalidate_dir(&mut self, path: &str) {
        self.invalidate_dir_in(&OperationContext::new(), path)
    }

    /// Invalidate a directory as part of an existing operation.
    pub fn invalidate_dir_in(&mut self, ctx: &OperationContext, path: &str) {
        let before = self.entries.len();
        let prefix = format!("{}/", path.trim_end_matches('/'));
        self.entries.retain(|e| !e.path.starts_with(&prefix));
        self.cached_at.retain(|p, _| !p.starts_with(&prefix));
        self.last_used.retain(|p, _| !p.starts_with(&prefix));
        self.memory_directory_owners
            .retain(|p, _| !p.starts_with(&prefix));
        self.emit(
            Some(ctx),
            Severity::Debug,
            "invalidate",
            "metadata cache directory invalidated",
            &[(
                "entries_removed",
                serde_json::json!(before - self.entries.len()),
            )],
        );
    }

    /// Replace the cached children of `dir` from a **multi-node** listing.
    ///
    /// Each element is one node's answer. The rule that keeps a partial
    /// outage from looking like a mass deletion:
    ///
    /// - [`ListingOutcome::Answered`] — this node's own entries that it did
    ///   not report are gone, and are dropped. Another node's entries are
    ///   *never* dropped on this node's say-so.
    /// - [`ListingOutcome::Unreachable`] — everything this node previously
    ///   contributed is **kept** and marked [`Availability::Inaccessible`].
    ///   `ls` keeps working; reads will fail.
    ///
    /// Returns the paths contributed by more than one node, in the order
    /// they were detected. Those are *not* merged: the first node in
    /// `outcomes` wins the name and the collision is reported so callers
    /// can surface it instead of silently picking a file.
    pub fn reconcile_from(
        &mut self,
        dir: &str,
        outcomes: Vec<(String, ListingOutcome)>,
    ) -> Vec<String> {
        self.reconcile_from_in(&OperationContext::new(), dir, outcomes)
    }

    /// Reconcile a listing as part of an existing operation.
    pub fn reconcile_from_in(
        &mut self,
        ctx: &OperationContext,
        dir: &str,
        outcomes: Vec<(String, ListingOutcome)>,
    ) -> Vec<String> {
        self.prune_expired();
        let Ok(initial) = self.rows_under(Some(ctx), dir, true) else {
            return Vec::new();
        };
        let mut rows = initial.clone();
        let mut displaced = CatalogPatch::new();
        let node_count = outcomes.len();
        let mut answered_count = 0usize;
        let mut unreachable_count = 0usize;
        let mut removed_count = 0usize;
        let mut recovered_count = 0usize;
        let mut conflicts = Vec::new();

        for (node_id, outcome) in outcomes {
            match outcome {
                ListingOutcome::Unreachable => {
                    unreachable_count += 1;
                    let mut count = 0;
                    let mut newly_inaccessible = 0;
                    for row in rows.values_mut().filter(|r| r.entry.node_id == node_id) {
                        count += 1;
                        newly_inaccessible +=
                            usize::from(row.entry.availability == Availability::Available);
                        row.entry.availability = Availability::Inaccessible;
                    }
                    self.emit(
                        Some(ctx),
                        Severity::Warning,
                        "reconcile",
                        "backing node did not answer; cached entries retained",
                        &[
                            ("node_id", serde_json::json!(&node_id)),
                            ("cached_entries", serde_json::json!(count)),
                            ("newly_inaccessible", serde_json::json!(newly_inaccessible)),
                        ],
                    );
                }
                ListingOutcome::Answered(children) => {
                    answered_count += 1;
                    let reported: HashSet<_> = children.iter().map(|e| e.path.clone()).collect();
                    // Keep the prior identities until this batch is applied,
                    // so an out-of-band rename can reuse the old inode.
                    let previous: HashMap<_, _> = rows
                        .values()
                        .filter(|r| {
                            r.entry.node_id == node_id && r.entry.kind != FsEntryKind::Directory
                        })
                        .map(|r| (r.entry.guid.clone(), r.entry.clone()))
                        .collect();
                    let stale: Vec<_> = rows
                        .values()
                        .filter(|r| {
                            r.entry.node_id == node_id || r.directory_owners.contains(&node_id)
                        })
                        .filter(|r| !reported.contains(r.entry.path.as_str()))
                        .map(|r| r.entry.path.clone())
                        .collect();
                    for path in stale {
                        let row = rows.remove(&path).unwrap();
                        if let Some(survivor) = remove_owner(row, &node_id) {
                            rows.insert(path, survivor);
                        } else {
                            removed_count += 1;
                        }
                    }
                    let mut node_recovered = 0;
                    let mut seen = HashSet::new();
                    for child in children {
                        if !seen.insert(child.path.clone()) {
                            continue;
                        }
                        if let Some(known) = rows.get_mut(&child.path)
                            && known.entry.node_id != node_id
                        {
                            if child.kind == FsEntryKind::Directory
                                && known.entry.kind == FsEntryKind::Directory
                            {
                                add_owner(&mut known.directory_owners, &node_id);
                            } else if !conflicts.contains(&child.path) {
                                conflicts.push(child.path.clone());
                            }
                            continue;
                        }

                        let existing = rows.get(&child.path);
                        let prior = if existing.is_none()
                            && child.kind != FsEntryKind::Directory
                            && !child.guid.is_empty()
                        {
                            previous
                                .get(&child.guid)
                                .cloned()
                                .or_else(|| {
                                    self.try_lookup_guid(Some(ctx), &node_id, &child.guid)
                                        .ok()
                                        .flatten()
                                })
                                .filter(|e| {
                                    e.kind != FsEntryKind::Directory
                                        && !reported.contains(e.path.as_str())
                                        && !displaced.contains_key(&e.path)
                                })
                        } else {
                            None
                        };
                        if let Some(prior) = &prior
                            && rows.remove(&prior.path).is_none()
                            && !initial.contains_key(&prior.path)
                        {
                            displaced.insert(prior.path.clone(), None);
                        }
                        let existing = rows.get(&child.path);
                        if child.availability == Availability::Available
                            && existing
                                .map(|r| &r.entry)
                                .or(prior.as_ref())
                                .is_some_and(|e| e.availability == Availability::Inaccessible)
                        {
                            node_recovered += 1;
                        }
                        let path = child.path.clone();
                        let row = self.make_row(child, existing, prior.as_ref());
                        rows.insert(path, row);
                    }
                    recovered_count += node_recovered;
                    if node_recovered > 0 {
                        self.emit(
                            Some(ctx),
                            Severity::Info,
                            "reconcile",
                            "cached entries became available again",
                            &[
                                ("node_id", serde_json::json!(&node_id)),
                                ("entries_recovered", serde_json::json!(node_recovered)),
                            ],
                        );
                    }
                }
            }
        }
        let mut patch = displaced;
        for path in initial.keys().filter(|path| !rows.contains_key(*path)) {
            patch.insert(path.clone(), None);
        }
        for (path, row) in rows {
            if initial.get(&path) != Some(&row) {
                patch.insert(path, Some(row));
            }
        }
        if self.admission_error().is_none() {
            self.apply(ctx, "reconcile", patch);
        }

        if answered_count == 0 && self.local_node.is_none() {
            self.emit(
                Some(ctx),
                Severity::Error,
                "reconcile",
                "no backing node answered the listing",
                &[
                    ("nodes", serde_json::json!(node_count)),
                    ("unreachable_nodes", serde_json::json!(unreachable_count)),
                ],
            );
        }
        if !conflicts.is_empty() {
            self.emit(
                Some(ctx),
                Severity::Warning,
                "reconcile",
                "conflicting names detected across nodes",
                &[("conflicts", serde_json::json!(conflicts.len()))],
            );
        }
        self.emit(
            Some(ctx),
            Severity::Debug,
            "reconcile",
            "metadata cache reconciliation completed",
            &[
                ("nodes_answered", serde_json::json!(answered_count)),
                ("nodes_unreachable", serde_json::json!(unreachable_count)),
                ("entries_removed", serde_json::json!(removed_count)),
                ("entries_recovered", serde_json::json!(recovered_count)),
                ("conflicts", serde_json::json!(conflicts.len())),
            ],
        );
        conflicts
    }

    pub fn get_children_of(&self, path: &str) -> Vec<&FsEntry> {
        // "/" already ends with a slash; "/foo" needs one added so "/foobar"
        // cannot match as a child of "/foo".
        let prefix = if path.ends_with('/') {
            path.to_owned()
        } else {
            format!("{path}/")
        };

        self.entries
            .iter()
            .filter(|e| {
                e.path
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
            })
            .collect()
    }
}

fn by_identity(same_path: Option<&CatalogRow>, previous: Option<&FsEntry>) -> Option<u64> {
    same_path
        .map(|row| row.entry.inode)
        .or_else(|| previous.map(|entry| entry.inode))
}

fn add_owner(owners: &mut Vec<String>, owner: &str) {
    if !owner.is_empty() && owners.iter().all(|id| id != owner) {
        owners.push(owner.to_owned());
    }
}

fn remove_owner(mut row: CatalogRow, owner: &str) -> Option<CatalogRow> {
    row.directory_owners.retain(|id| id != owner);
    if row.entry.node_id == owner {
        row.entry.node_id = row.directory_owners.first()?.clone();
    }
    Some(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(inode: u64, path: &str, guid: &str, kind: FsEntryKind) -> FsEntry {
        FsEntry {
            inode,
            path: path.to_owned(),
            guid: guid.to_owned(),
            kind,
            node_id: "n".into(),
            availability: Availability::Available,
            attr: FileAttrData {
                guid: guid.to_owned(),
                kind,
                ..FileAttrData::default()
            },
        }
    }

    fn tree() -> MetadataCache {
        let mut tree = MetadataCache::new();
        tree.insert_root("/", "root-guid", FsEntryKind::Directory);
        tree.upsert_entry(entry(0, "/docs", "guid-docs", FsEntryKind::Directory));
        tree.upsert_entry(entry(0, "/docs/a", "guid-a", FsEntryKind::File));
        tree.upsert_entry(entry(0, "/docs/sub", "guid-sub", FsEntryKind::Directory));
        tree.upsert_entry(entry(0, "/docs/sub/b", "guid-b", FsEntryKind::File));
        tree
    }

    fn paths(entries: Vec<&FsEntry>) -> Vec<String> {
        let mut paths: Vec<String> = entries.into_iter().map(|e| e.path.clone()).collect();
        paths.sort();
        paths
    }

    #[test]
    fn children_are_direct_descendants_only() {
        let tree = tree();
        assert_eq!(
            paths(tree.get_children_of("/docs")),
            vec!["/docs/a", "/docs/sub"]
        );
        assert_eq!(paths(tree.get_children_of("/")), vec!["/docs"]);
        assert_eq!(
            paths(tree.get_children_of("/docs/sub")),
            vec!["/docs/sub/b"]
        );
    }

    #[test]
    fn sibling_prefix_is_not_a_child() {
        let mut tree = tree();
        tree.upsert_entry(entry(0, "/docsx", "guid-x", FsEntryKind::File));
        assert!(
            tree.get_children_of("/docs")
                .iter()
                .all(|e| e.path != "/docsx")
        );
    }

    #[test]
    fn has_children_tracks_direct_children() {
        let tree = tree();
        assert!(tree.has_children("/docs"));
        assert!(!tree.has_children("/docs/a"));
        assert!(!tree.has_children("/nope"));
    }

    #[test]
    fn lookup_helpers_round_trip() {
        let tree = tree();
        let a = tree.get_inode("/docs/a").unwrap();
        assert_eq!(tree.get_path(a), Some("/docs/a"));
        assert_eq!(
            tree.get_entry_by_guid("guid-a").map(|e| e.path.as_str()),
            Some("/docs/a")
        );
        assert_eq!(tree.get_inode("/nope"), None);
    }

    #[test]
    fn inodes_are_unique_and_never_reused() {
        let mut tree = MetadataCache::new();
        tree.insert_root("/", "root", FsEntryKind::Directory);
        tree.upsert_entry(entry(0, "/a", "guid-a", FsEntryKind::File));
        tree.upsert_entry(entry(0, "/b", "guid-b", FsEntryKind::File));
        let a = tree.get_inode("/a").unwrap();
        let b = tree.get_inode("/b").unwrap();
        assert_ne!(a, b);

        // Removing and re-adding must not hand out the retired inode.
        tree.remove_entry("/a");
        tree.upsert_entry(entry(0, "/a", "guid-a2", FsEntryKind::File));
        assert_ne!(tree.get_inode("/a").unwrap(), a);
    }

    #[test]
    fn upsert_matches_by_guid_so_a_rename_keeps_the_inode() {
        let mut tree = tree();
        let inode = tree.get_inode("/docs/a").unwrap();

        // Same guid, new path: the reconciler must reuse the inode.
        tree.upsert_entry(entry(0, "/docs/renamed", "guid-a", FsEntryKind::File));
        assert_eq!(tree.get_inode("/docs/renamed"), Some(inode));
        assert_eq!(tree.get_inode("/docs/a"), None);
        assert_eq!(tree.entries.len(), 5, "no duplicate row for the old path");
    }

    #[test]
    fn upsert_matches_by_path_when_the_guid_changes() {
        let mut tree = tree();
        let inode = tree.get_inode("/docs/a").unwrap();
        tree.upsert_entry(entry(0, "/docs/a", "guid-different", FsEntryKind::File));
        // Path wins, so the inode is stable for the kernel; guid follows the
        // server's new answer.
        assert_eq!(tree.get_inode("/docs/a"), Some(inode));
        assert_eq!(tree.get_entry_by_guid("guid-a"), None);
    }

    #[test]
    fn remove_entry_drops_the_subtree() {
        let mut tree = tree();
        tree.remove_entry("/docs");
        assert!(tree.get_entry_by_path("/docs").is_none());
        assert!(tree.get_entry_by_path("/docs/a").is_none());
        assert!(tree.get_entry_by_path("/docs/sub/b").is_none());
        // Root is untouched.
        assert!(tree.get_entry_by_path("/").is_some());
    }

    #[test]
    fn rename_entry_moves_the_subtree() {
        let mut tree = tree();
        let inode = tree.get_inode("/docs/sub/b").unwrap();
        assert!(tree.rename_entry("/docs/sub", "/docs/moved"));
        assert_eq!(tree.get_inode("/docs/moved/b"), Some(inode));
        assert!(tree.get_entry_by_path("/docs/sub/b").is_none());
    }

    #[test]
    fn rename_entry_reports_unknown_sources() {
        let mut tree = tree();
        assert!(!tree.rename_entry("/nope", "/elsewhere"));
    }

    #[test]
    fn invalidate_dir_keeps_the_directory_itself() {
        let mut tree = tree();
        tree.invalidate_dir("/docs");
        assert!(tree.get_entry_by_path("/docs").is_some());
        assert!(tree.get_entry_by_path("/docs/a").is_none());
        assert!(tree.get_entry_by_path("/docs/sub/b").is_none());
    }

    /// Build an entry owned by a specific node.
    fn owned(inode: u64, path: &str, guid: &str, node: &str) -> FsEntry {
        let mut e = entry(inode, path, guid, FsEntryKind::File);
        e.node_id = node.to_owned();
        e
    }

    fn dir_owned(inode: u64, path: &str, guid: &str, node: &str) -> FsEntry {
        let mut e = entry(inode, path, guid, FsEntryKind::Directory);
        e.node_id = node.to_owned();
        e
    }

    /// A node that answered drops its own unreported entries.
    #[test]
    fn an_answer_delete_only_the_answering_nodes_own_entries() {
        let mut tree = MetadataCache::new();
        tree.insert_root("/", "root", FsEntryKind::Directory);
        tree.upsert_entry(owned(0, "/a.txt", "guid-a", "node-a"));
        tree.upsert_entry(owned(0, "/b.txt", "guid-b", "node-b"));

        // node-a answers with nothing: only its own entry may go.
        let conflicts = tree.reconcile_from(
            "/",
            vec![("node-a".into(), ListingOutcome::Answered(vec![]))],
        );

        assert!(conflicts.is_empty());
        assert!(
            tree.get_entry_by_path("/a.txt").is_none(),
            "node-a's entry is gone"
        );
        assert!(
            tree.get_entry_by_path("/b.txt").is_some(),
            "node-b's entry must survive another node's listing"
        );
    }

    /// The safety property: an unreachable node keeps everything it
    /// contributed, marked inaccessible.
    #[test]
    fn an_unreachable_node_keeps_its_entries_and_marks_them_inaccessible() {
        let mut tree = MetadataCache::new();
        tree.insert_root("/", "root", FsEntryKind::Directory);
        tree.upsert_entry(owned(0, "/a.txt", "guid-a", "node-a"));
        tree.upsert_entry(owned(0, "/b.txt", "guid-b", "node-b"));

        tree.reconcile_from(
            "/",
            vec![
                ("node-a".into(), ListingOutcome::Answered(vec![])),
                ("node-b".into(), ListingOutcome::Unreachable),
            ],
        );

        assert!(
            tree.get_entry_by_path("/a.txt").is_none(),
            "answering node's entry dropped"
        );
        let kept = tree
            .get_entry_by_path("/b.txt")
            .expect("an outage must not delete entries");
        assert_eq!(kept.availability, Availability::Inaccessible);
    }

    /// A reachable node's entries come back to Available after an outage.
    #[test]
    fn an_entry_becomes_available_again_when_its_node_returns() {
        let mut tree = MetadataCache::new();
        tree.insert_root("/", "root", FsEntryKind::Directory);
        tree.upsert_entry(owned(0, "/a.txt", "guid-a", "node-a"));

        tree.reconcile_from("/", vec![("node-a".into(), ListingOutcome::Unreachable)]);
        assert_eq!(
            tree.get_entry_by_path("/a.txt").unwrap().availability,
            Availability::Inaccessible
        );

        tree.reconcile_from(
            "/",
            vec![(
                "node-a".into(),
                ListingOutcome::Answered(vec![owned(0, "/a.txt", "guid-a", "node-a")]),
            )],
        );
        assert_eq!(
            tree.get_entry_by_path("/a.txt").unwrap().availability,
            Availability::Available
        );
    }

    /// A directory contributed by two nodes is a merge point, not a
    /// collision: it is reported once, both sides' children survive.
    #[test]
    fn a_shared_directory_merges_and_is_not_a_conflict() {
        let mut tree = MetadataCache::new();
        tree.insert_root("/", "root", FsEntryKind::Directory);

        tree.reconcile_from(
            "/",
            vec![
                (
                    "node-a".into(),
                    ListingOutcome::Answered(vec![dir_owned(0, "/shared", "guid-sa", "node-a")]),
                ),
                (
                    "node-b".into(),
                    ListingOutcome::Answered(vec![dir_owned(0, "/shared", "guid-sb", "node-b")]),
                ),
            ],
        );

        assert!(
            tree.get_children_of("/")
                .iter()
                .filter(|e| e.path == "/shared")
                .count()
                == 1,
            "a shared directory must appear once"
        );

        // Both sides contribute children.
        tree.reconcile_from(
            "/shared",
            vec![
                (
                    "node-a".into(),
                    ListingOutcome::Answered(vec![owned(0, "/shared/a.txt", "guid-a", "node-a")]),
                ),
                (
                    "node-b".into(),
                    ListingOutcome::Answered(vec![owned(0, "/shared/b.txt", "guid-b", "node-b")]),
                ),
            ],
        );

        let mut names: Vec<String> = tree
            .get_children_of("/shared")
            .iter()
            .map(|e| e.path.clone())
            .collect();
        names.sort();
        assert_eq!(names, vec!["/shared/a.txt", "/shared/b.txt"]);
    }

    /// Two *files* with one name is a genuine collision: reported, and the
    /// first node by precedence wins.
    #[test]
    fn two_files_with_one_name_are_reported_as_a_conflict() {
        let mut tree = MetadataCache::new();
        tree.insert_root("/", "root", FsEntryKind::Directory);

        let conflicts = tree.reconcile_from(
            "/",
            vec![
                (
                    "node-a".into(),
                    ListingOutcome::Answered(vec![owned(0, "/dup.txt", "guid-a", "node-a")]),
                ),
                (
                    "node-b".into(),
                    ListingOutcome::Answered(vec![owned(0, "/dup.txt", "guid-b", "node-b")]),
                ),
            ],
        );

        assert_eq!(conflicts, vec!["/dup.txt"]);
        // First by precedence, deterministically.
        assert_eq!(
            tree.get_entry_by_path("/dup.txt").unwrap().node_id,
            "node-a"
        );
    }

    /// A child's inode is stable across listings while its guid is unchanged.
    #[test]
    fn reconcile_keeps_inodes_for_unchanged_guids() {
        let mut tree = MetadataCache::new();
        tree.insert_root("/", "root", FsEntryKind::Directory);
        tree.reconcile_from(
            "/",
            vec![(
                "node-a".into(),
                ListingOutcome::Answered(vec![owned(0, "/a.txt", "guid-a", "node-a")]),
            )],
        );
        let inode = tree.get_inode("/a.txt").unwrap();

        tree.reconcile_from(
            "/",
            vec![(
                "node-a".into(),
                ListingOutcome::Answered(vec![owned(0, "/a.txt", "guid-a", "node-a")]),
            )],
        );
        assert_eq!(tree.get_inode("/a.txt"), Some(inode));
    }
}
