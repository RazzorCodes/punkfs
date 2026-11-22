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
use std::time::{Duration, SystemTime};

use super::attributes::{FileAttrData, FsEntryKind};
use crate::telemetry::{self, Event, Logger, OperationContext, Severity};

pub struct FsNode {
    pub node_id: String,
}

pub struct FsNodeMetadata {
    pub parent_node: String,
    pub guid: String,
}

/// Whether the node that owns an entry is currently reachable.
///
/// An entry is deliberately **kept** in the catalog when its node goes
/// away: the namespace stays browsable (`ls`, `stat`) and only the
/// operations that need the bytes fail. Dropping the entry instead would
/// make a transient outage look like a mass deletion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Availability {
    #[default]
    Available,
    /// Owning node has not answered recently; content ops will fail.
    Inaccessible,
}

/// One entry in the local catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsEntry {
    /// Locally allocated FUSE inode. Meaningless off this machine.
    pub inode: u64,
    pub path: String,
    /// punkfs identity, from the server.
    pub guid: String,
    pub kind: FsEntryKind,
    /// Where the authoritative bytes live.
    pub node_id: String,
    /// Whether that node is currently answering.
    pub availability: Availability,
    pub attr: FileAttrData,
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

    fn enabled(&self, severity: Severity) -> bool {
        self.logger
            .as_ref()
            .or_else(|| telemetry::global())
            .is_some_and(|logger| logger.enabled(severity))
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
    pub fn upsert_entry_in(&mut self, ctx: &OperationContext, entry: FsEntry) -> Option<FsEntry> {
        let mut entry = entry;
        if entry.inode == 0 {
            // Reuse the inode of the entry this one is replacing, matched
            // by guid (a rename) or by path (the same file with a new
            // server-assigned guid). Only a genuinely new entry gets a
            // fresh inode.
            let known = self
                .entries
                .iter()
                .find(|e| e.guid == entry.guid || e.path == entry.path)
                .map(|e| e.inode);
            entry.inode = known.unwrap_or_else(|| self.alloc_inode());
        } else {
            self.next_inode = self.next_inode.max(entry.inode + 1);
        }

        let existing = self
            .entries
            .iter()
            .position(|e| e.guid == entry.guid || e.path == entry.path);

        let old = match existing {
            Some(idx) => {
                let old = std::mem::replace(&mut self.entries[idx], entry);
                // A rename can leave a duplicate row for the old path.
                self.entries.dedup_by(|a, b| a.inode == b.inode);
                Some(old)
            }
            None => {
                self.entries.push(entry);
                None
            }
        };
        if self.enabled(Severity::Trace) {
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
        }
        old
    }

    /// Seed an entry at a known inode, used for the root at startup.
    pub fn insert_root(&mut self, path: &str, guid: &str, kind: FsEntryKind) {
        let inode = self.alloc_inode();
        self.entries.push(FsEntry {
            inode,
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
    ///
    /// Used when the server reports a removal; the kernel's cached inodes
    /// are invalidated separately by the caller.
    pub fn remove_entry(&mut self, path: &str) -> Option<FsEntry> {
        self.remove_entry_in(&OperationContext::new(), path)
    }

    /// Remove an entry as part of an existing operation.
    pub fn remove_entry_in(&mut self, ctx: &OperationContext, path: &str) -> Option<FsEntry> {
        let before = self.entries.len();
        let removed = self
            .entries
            .iter()
            .position(|e| e.path == path)
            .map(|idx| self.entries.remove(idx));
        if removed.is_some() {
            let prefix = format!("{}/", path.trim_end_matches('/'));
            self.entries.retain(|e| !e.path.starts_with(&prefix));
        }
        self.emit(
            Some(ctx),
            Severity::Debug,
            "remove",
            "metadata cache removal completed",
            &[
                (
                    "outcome",
                    serde_json::json!(if removed.is_some() {
                        "removed"
                    } else {
                        "not_cached"
                    }),
                ),
                (
                    "entries_removed",
                    serde_json::json!(before - self.entries.len()),
                ),
            ],
        );
        removed
    }

    /// Move `old` (and its subtree) to `new`, keeping inodes and guids.
    ///
    /// This only rewrites our local view; the authoritative move happened
    /// on the server. Returns false if `old` was not known.
    pub fn rename_entry(&mut self, old: &str, new: &str) -> bool {
        self.rename_entry_in(&OperationContext::new(), old, new)
    }

    /// Rename cached entries as part of an existing operation.
    pub fn rename_entry_in(&mut self, ctx: &OperationContext, old: &str, new: &str) -> bool {
        let mut moved = false;
        let mut moved_entries = 0;
        let old_prefix = format!("{}/", old.trim_end_matches('/'));

        for entry in &mut self.entries {
            if entry.path == old {
                entry.path = new.to_owned();
                moved = true;
                moved_entries += 1;
            } else if let Some(rest) = entry.path.strip_prefix(&old_prefix) {
                entry.path = format!("{}/{rest}", new.trim_end_matches('/'));
                moved = true;
                moved_entries += 1;
            }
        }
        self.emit(
            Some(ctx),
            Severity::Debug,
            "rename",
            "metadata cache rename completed",
            &[
                (
                    "outcome",
                    serde_json::json!(if moved { "renamed" } else { "not_cached" }),
                ),
                ("entries_renamed", serde_json::json!(moved_entries)),
            ],
        );
        moved
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
        let node_count = outcomes.len();
        let mut answered_count = 0usize;
        let mut unreachable_count = 0usize;
        let mut removed_count = 0usize;
        let mut recovered_count = 0usize;
        let observe_recovery = self.enabled(Severity::Info);
        let dir_prefix = if dir.ends_with('/') {
            dir.to_owned()
        } else {
            format!("{dir}/")
        };
        let direct_child = |path: &str| {
            path.strip_prefix(&dir_prefix)
                .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
        };

        let mut conflicts: Vec<String> = Vec::new();
        let mut claimed: Vec<String> = Vec::new();

        for (node_id, outcome) in outcomes {
            match outcome {
                ListingOutcome::Unreachable => {
                    unreachable_count += 1;
                    // Keep this node's entries, but stop pretending they are
                    // readable.
                    let newly_inaccessible = self
                        .entries
                        .iter()
                        .filter(|e| {
                            e.node_id == node_id
                                && direct_child(&e.path)
                                && e.availability == Availability::Available
                        })
                        .count();
                    let paths: Vec<String> = self
                        .entries
                        .iter()
                        .filter(|e| e.node_id == node_id && direct_child(&e.path))
                        .map(|e| e.path.clone())
                        .collect();
                    self.emit(
                        Some(ctx),
                        Severity::Warning,
                        "reconcile",
                        "backing node did not answer; cached entries retained",
                        &[
                            ("node_id", serde_json::json!(&node_id)),
                            ("cached_entries", serde_json::json!(paths.len())),
                            ("newly_inaccessible", serde_json::json!(newly_inaccessible)),
                        ],
                    );
                    for path in paths {
                        let survivors: Vec<FsEntry> = self
                            .entries
                            .iter()
                            .filter(|e| e.path == path)
                            .map(|e| {
                                let mut kept = e.clone();
                                if kept.node_id == node_id {
                                    kept.availability = Availability::Inaccessible;
                                }
                                kept
                            })
                            .collect();
                        self.entries.retain(|e| e.path != path);
                        for entry in survivors {
                            self.entries.push(entry);
                        }
                    }
                }
                ListingOutcome::Answered(children) => {
                    answered_count += 1;
                    let mut node_recovered = 0usize;
                    let reported: Vec<&str> = children.iter().map(|c| c.guid.as_str()).collect();

                    // Only this node's own unreported entries are provably gone.
                    let stale: Vec<String> = self
                        .entries
                        .iter()
                        .filter(|e| e.node_id == node_id && direct_child(&e.path))
                        .filter(|e| !reported.contains(&e.guid.as_str()))
                        .map(|e| e.path.clone())
                        .collect();
                    let before = self.entries.len();
                    self.entries
                        .retain(|e| !(e.node_id == node_id && stale.iter().any(|s| s == &e.path)));
                    removed_count += before - self.entries.len();

                    for child in children {
                        let also = self
                            .entries
                            .iter()
                            .any(|e| e.path == child.path && e.node_id != child.node_id);

                        if also {
                            // Two nodes have a name in common. Directories
                            // merge: both sides contribute children, which
                            // is the whole point of a merged namespace.
                            // Anything else is a genuine collision — two
                            // different files claiming one name — and must
                            // not be resolved silently.
                            if child.kind == FsEntryKind::Directory
                                && self.entries.iter().any(|e| {
                                    e.path == child.path && e.kind == FsEntryKind::Directory
                                })
                            {
                                continue;
                            }
                            if !conflicts.contains(&child.path) {
                                conflicts.push(child.path.clone());
                            }
                            continue;
                        }

                        if observe_recovery
                            && child.availability == Availability::Available
                            && self.entries.iter().any(|entry| {
                                entry.node_id == node_id
                                    && entry.availability == Availability::Inaccessible
                                    && (entry.guid == child.guid || entry.path == child.path)
                            })
                        {
                            node_recovered += 1;
                        }
                        claimed.push(child.path.clone());
                        self.upsert_entry_in(ctx, child);
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

        if answered_count == 0 {
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
