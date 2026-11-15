//! The client-side catalog: inodes, paths, and last-known metadata.
//!
//! Two distinct identities are in play:
//!
//! - **`uuid`** is the punkfs identity of an entry. It is what the catalog
//!   (and later the coordinator) uses to say "this is the same file", so it
//!   survives renames, re-listings, and a client restart.
//! - **`inode`** is a purely local FUSE convenience: the kernel needs a
//!   `u64` and we need something cheap to index entries by. It is allocated
//!   by this client and means nothing outside it.
//!
//! Entries are therefore looked up by `uuid` when we reconcile a listing,
//! and by `inode`/`path` when the kernel asks about something.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct FsNode {
    pub node_id: String,
}

pub struct FsNodeMetadata {
    pub parent_node: String,
    pub uuid: String,
}

/// The four POSIX timestamps, each defaulting to the epoch.
///
/// `Default` is hand-written because `SystemTime` does not implement it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileTimes {
    pub accessed: SystemTime,
    pub modified: SystemTime,
    pub changed: SystemTime,
    pub created: SystemTime,
}

impl Default for FileTimes {
    fn default() -> Self {
        FileTimes {
            accessed: UNIX_EPOCH,
            modified: UNIX_EPOCH,
            changed: UNIX_EPOCH,
            created: UNIX_EPOCH,
        }
    }
}

impl FileTimes {
    /// Build from the second-resolution values a `stat` gives us.
    pub fn from_secs(
        atime: u64,
        mtime: u64,
        ctime: u64,
        crtime: u64,
    ) -> Self {
        let epoch = |s: u64| UNIX_EPOCH + Duration::from_secs(s);
        FileTimes {
            accessed: epoch(atime),
            modified: epoch(mtime),
            changed: epoch(ctime),
            created: epoch(crtime),
        }
    }
}

/// Ownership and permission bits (`chmod`/`chown` territory).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Access {
    /// Permission bits, e.g. `0o755`. Zero means "unknown, not "no access"".
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
}

/// Size and layout counters (`stat`, `du`, `df` territory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub blocks: u32,
    pub block_size: u16,
    /// Device number, only meaningful for device nodes.
    pub rdev: u16,
    pub nlink: u32,
    /// BSD-style file flags.
    pub flags: u32,
}

impl Default for Stat {
    fn default() -> Self {
        // block_size defaults to 512 rather than 0: `st_blksize` of zero
        // makes some programs divide by zero when computing block counts.
        Stat {
            size: 0,
            blocks: 0,
            block_size: 512,
            rdev: 0,
            nlink: 0,
            flags: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FsEntryKind {
    #[default]
    Unknown,
    Directory,
    File,
    Link,
}

/// The stat-shaped payload of an entry: everything except identity and
/// location. Mirrors the proto `FileAttr` one-to-one so conversions are
/// mechanical.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileAttrData {
    /// punkfs identity; stable across renames once the catalog owns it.
    pub uuid: String,
    pub kind: FsEntryKind,
    pub times: FileTimes,
    pub access: Access,
    pub stat: Stat,
}

/// One entry in the local catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsEntry {
    /// Locally allocated FUSE inode. Meaningless off this machine.
    pub inode: u64,
    pub path: String,
    /// punkfs identity, from the server.
    pub uuid: String,
    pub kind: FsEntryKind,
    /// Where the authoritative bytes live.
    pub node_id: String,
    pub attr: FileAttrData,
}

pub struct FsTree {
    pub entries: Vec<FsEntry>,
    next_inode: u64,
}

impl Default for FsTree {
    fn default() -> Self {
        FsTree::new()
    }
}

impl FsTree {
    pub fn new() -> Self {
        FsTree {
            entries: Vec::new(),
            next_inode: 1,
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

    pub fn get_entry_by_uuid(&self, uuid: &str) -> Option<&FsEntry> {
        self.entries.iter().find(|e| e.uuid == uuid)
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
    /// Matching order is **uuid first, then path**, which is what keeps a
    /// renamed file pointing at the same local inode: the reconciler sees a
    /// new path carrying a known uuid and updates the existing entry rather
    /// than allocating a second one.
    ///
    /// - `inode == 0` means "allocate a local inode".
    /// - `metadata` replaces the stored attributes wholesale.
    ///
    /// Returns the old entry if one was replaced.
    pub fn upsert_entry(&mut self, entry: FsEntry) -> Option<FsEntry> {
        let mut entry = entry;
        if entry.inode == 0 {
            // Reuse the inode of the entry this one is replacing, matched
            // by uuid (a rename) or by path (the same file with a new
            // server-assigned uuid). Only a genuinely new entry gets a
            // fresh inode.
            let known = self
                .entries
                .iter()
                .find(|e| e.uuid == entry.uuid || e.path == entry.path)
                .map(|e| e.inode);
            entry.inode = known.unwrap_or_else(|| self.alloc_inode());
        } else {
            self.next_inode = self.next_inode.max(entry.inode + 1);
        }

        let existing = self
            .entries
            .iter()
            .position(|e| e.uuid == entry.uuid || e.path == entry.path);

        match existing {
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
        }
    }

    /// Seed an entry at a known inode, used for the root at startup.
    pub fn insert_root(&mut self, path: &str, uuid: &str, kind: FsEntryKind) {
        let inode = self.alloc_inode();
        self.entries.push(FsEntry {
            inode,
            path: path.to_owned(),
            uuid: uuid.to_owned(),
            kind,
            node_id: String::new(),
            attr: FileAttrData {
                uuid: uuid.to_owned(),
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
        let removed = self
            .entries
            .iter()
            .position(|e| e.path == path)
            .map(|idx| self.entries.remove(idx))?;

        let prefix = format!("{}/", path.trim_end_matches('/'));
        self.entries.retain(|e| !e.path.starts_with(&prefix));
        Some(removed)
    }

    /// Move `old` (and its subtree) to `new`, keeping inodes and uuids.
    ///
    /// This only rewrites our local view; the authoritative move happened
    /// on the server. Returns false if `old` was not known.
    pub fn rename_entry(&mut self, old: &str, new: &str) -> bool {
        let mut moved = false;
        let old_prefix = format!("{}/", old.trim_end_matches('/'));

        for entry in &mut self.entries {
            if entry.path == old {
                entry.path = new.to_owned();
                moved = true;
            } else if let Some(rest) = entry.path.strip_prefix(&old_prefix) {
                entry.path = format!("{}/{rest}", new.trim_end_matches('/'));
                moved = true;
            }
        }
        moved
    }

    /// Forget everything cached below `path`, keeping `path` itself.
    ///
    /// Called after a mutation so the next `readdir` refetches instead of
    /// serving entries the server has already changed.
    pub fn invalidate_dir(&mut self, path: &str) {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        self.entries.retain(|e| !e.path.starts_with(&prefix));
    }

    /// Replace the cached children of `dir` with `children`.
    ///
    /// Only called after a *successful* listing, which is what makes
    /// "absent from the response" a safe deletion signal. Entries are
    /// matched by uuid, so an unchanged file keeps its local inode.
    pub fn reconcile(&mut self, dir: &str, children: Vec<FsEntry>) {
        let known = self.get_children_of(dir);
        // Children arrive with `inode == 0` (the server never sends an
        // inode), so survival is decided by uuid, not by inode.
        let keep: Vec<&str> = children.iter().map(|c| c.uuid.as_str()).collect();

        // Drop cached children the server no longer reports.
        let dir_prefix = if dir.ends_with('/') {
            dir.to_owned()
        } else {
            format!("{dir}/")
        };
        let stale: Vec<String> = known
            .iter()
            .filter(|e| !keep.contains(&e.uuid.as_str()))
            .filter(|e| {
                e.path
                    .strip_prefix(&dir_prefix)
                    .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
            })
            .map(|e| e.path.clone())
            .collect();
        self.entries
            .retain(|e| !stale.iter().any(|s| &e.path == s));

        for child in children {
            self.upsert_entry(child);
        }
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

    fn entry(inode: u64, path: &str, uuid: &str, kind: FsEntryKind) -> FsEntry {
        FsEntry {
            inode,
            path: path.to_owned(),
            uuid: uuid.to_owned(),
            kind,
            node_id: "n".into(),
            attr: FileAttrData {
                uuid: uuid.to_owned(),
                kind,
                ..FileAttrData::default()
            },
        }
    }

    fn tree() -> FsTree {
        let mut tree = FsTree::new();
        tree.insert_root("/", "root-uuid", FsEntryKind::Directory);
        tree.upsert_entry(entry(0, "/docs", "uuid-docs", FsEntryKind::Directory));
        tree.upsert_entry(entry(0, "/docs/a", "uuid-a", FsEntryKind::File));
        tree.upsert_entry(entry(0, "/docs/sub", "uuid-sub", FsEntryKind::Directory));
        tree.upsert_entry(entry(0, "/docs/sub/b", "uuid-b", FsEntryKind::File));
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
        assert_eq!(paths(tree.get_children_of("/docs")), vec!["/docs/a", "/docs/sub"]);
        assert_eq!(paths(tree.get_children_of("/")), vec!["/docs"]);
        assert_eq!(paths(tree.get_children_of("/docs/sub")), vec!["/docs/sub/b"]);
    }

    #[test]
    fn sibling_prefix_is_not_a_child() {
        let mut tree = tree();
        tree.upsert_entry(entry(0, "/docsx", "uuid-x", FsEntryKind::File));
        assert!(tree.get_children_of("/docs").iter().all(|e| e.path != "/docsx"));
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
        assert_eq!(tree.get_entry_by_uuid("uuid-a").map(|e| e.path.as_str()), Some("/docs/a"));
        assert_eq!(tree.get_inode("/nope"), None);
    }

    #[test]
    fn inodes_are_unique_and_never_reused() {
        let mut tree = FsTree::new();
        tree.insert_root("/", "root", FsEntryKind::Directory);
        tree.upsert_entry(entry(0, "/a", "uuid-a", FsEntryKind::File));
        tree.upsert_entry(entry(0, "/b", "uuid-b", FsEntryKind::File));
        let a = tree.get_inode("/a").unwrap();
        let b = tree.get_inode("/b").unwrap();
        assert_ne!(a, b);

        // Removing and re-adding must not hand out the retired inode.
        tree.remove_entry("/a");
        tree.upsert_entry(entry(0, "/a", "uuid-a2", FsEntryKind::File));
        assert_ne!(tree.get_inode("/a").unwrap(), a);
    }

    #[test]
    fn upsert_matches_by_uuid_so_a_rename_keeps_the_inode() {
        let mut tree = tree();
        let inode = tree.get_inode("/docs/a").unwrap();

        // Same uuid, new path: the reconciler must reuse the inode.
        tree.upsert_entry(entry(0, "/docs/renamed", "uuid-a", FsEntryKind::File));
        assert_eq!(tree.get_inode("/docs/renamed"), Some(inode));
        assert_eq!(tree.get_inode("/docs/a"), None);
        assert_eq!(tree.entries.len(), 5, "no duplicate row for the old path");
    }

    #[test]
    fn upsert_matches_by_path_when_the_uuid_changes() {
        let mut tree = tree();
        let inode = tree.get_inode("/docs/a").unwrap();
        tree.upsert_entry(entry(0, "/docs/a", "uuid-different", FsEntryKind::File));
        // Path wins, so the inode is stable for the kernel; uuid follows the
        // server's new answer.
        assert_eq!(tree.get_inode("/docs/a"), Some(inode));
        assert_eq!(tree.get_entry_by_uuid("uuid-a"), None);
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

    #[test]
    fn reconcile_keeps_inodes_by_uuid_and_drops_stale_children() {
        let mut tree = tree();
        let a_inode = tree.get_inode("/docs/a").unwrap();

        tree.reconcile(
            "/docs",
            vec![
                entry(0, "/docs/a", "uuid-a", FsEntryKind::File),
                entry(0, "/docs/new", "uuid-new", FsEntryKind::File),
            ],
        );

        // Survived: same uuid, same inode.
        assert_eq!(tree.get_inode("/docs/a"), Some(a_inode));
        // Added.
        assert!(tree.get_entry_by_path("/docs/new").is_some());
        // Gone: the server did not report it.
        assert!(tree.get_entry_by_path("/docs/sub").is_none());
    }

    #[test]
    fn reconcile_does_not_touch_unrelated_directories() {
        let mut tree = tree();
        tree.reconcile("/docs", vec![entry(0, "/docs/a", "uuid-a", FsEntryKind::File)]);
        // "/" and its own children are outside the reconciled directory.
        assert!(tree.get_entry_by_path("/").is_some());
    }

    #[test]
    fn file_times_convert_from_seconds() {
        let times = FileTimes::from_secs(1, 2, 3, 4);
        assert_eq!(times.accessed, UNIX_EPOCH + Duration::from_secs(1));
        assert_eq!(times.created, UNIX_EPOCH + Duration::from_secs(4));
    }

    #[test]
    fn default_stat_has_a_usable_block_size() {
        assert_eq!(Stat::default().block_size, 512);
    }
}
