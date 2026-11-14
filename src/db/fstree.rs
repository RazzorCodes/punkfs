pub struct FsNode {
    pub node_id: String,
}

pub struct FsNodeMetadata {
    pub parent_node: String,
    pub uuid: String,
}

/// The stat-shaped metadata carried by every entry. All fields default
/// to zero, so `FsMetadata::default()` is the "empty" metadata.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsMetadata {
    pub size: u64,    // filesize
    pub blocks: u32,  // size in blocks
    pub blksize: u16, // size of blocks
    pub rdev: u16,    // device number (only meaningful for device nodes)
    // --- timestamps
    pub atime: u64,  // last access/read
    pub mtime: u64,  // last modified
    pub ctime: u64,  // last inode/path change
    pub crtime: u64, // creation time
    // --- owner & perms
    pub perm: u16,  // file permissions, octal (e.g. 0o777)
    pub uid: u32,   // owner user id
    pub gid: u32,   // group id
    pub nlink: u32, // number of hard links
    pub flags: u32, // BSD-style flags
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsEntryKind {
    Unknown,
    Directory,
    File,
    Link,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsEntry {
    pub inode: u64,
    pub path: String,
    pub kind: FsEntryKind,
    pub metadata: FsMetadata,
    pub node_id: String, // owned by the node authoritative for this path
}

pub struct FsTree {
    pub entries: Vec<FsEntry>,
    // pub nodes: Vec<FsNode>,
}

impl FsTree {
    pub fn get_entry_by_path(&self, path: &str) -> Option<&FsEntry> {
        return self.entries.iter().find(|e| e.path == path);
    }
    pub fn get_inode(&self, path: &str) -> Option<u64> {
        self.entries
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.inode)
    }

    pub fn get_path(&self, inode: u64) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.inode == inode)
            .map(|e| e.path.as_str())
    }

    /// Insert or update the entry identified by `inode`/`path`.
    ///
    /// - `inode == 0` means "allocate the next free inode".
    /// - `path == ""` means "keep the path already stored for this inode".
    /// - `metadata == None` means "keep whatever metadata the entry had";
    ///   a freshly inserted entry with `None` gets `FsMetadata::default()`.
    /// - `kind == Unknown` means "keep whatever kind the entry had"; a
    ///   freshly inserted entry with `Unknown` stays `Unknown` (callers
    ///   fall back to treating it as a file).
    ///
    /// Returns the old entry if this upsert *replaced* one, `None` if it
    /// was a plain insert.
    pub fn upsert_entry(
        &mut self,
        path: &str,
        inode: u64,
        kind: FsEntryKind,
        metadata: Option<FsMetadata>,
    ) -> Option<FsEntry> {
        let inode = if inode == 0 {
            self.entries.iter().map(|e| e.inode).max().unwrap_or(1) + 1
        } else {
            inode
        };

        let path = if path.is_empty() {
            self.entries.iter().find(|e| e.inode == inode)?.path.clone()
        } else {
            path.to_owned()
        };

        // Old metadata/kind survive across an update; a brand-new entry
        // gets zeros / Unknown (or the caller-provided values).
        let old = self
            .entries
            .iter()
            .find(|e| e.inode == inode || e.path == path);
        let old_metadata = old.map(|e| e.metadata);
        let old_kind = old.map(|e| e.kind).unwrap_or(FsEntryKind::Unknown);
        let old_node_id = old.map(|e| e.node_id.clone());
        let metadata = metadata.or(old_metadata).unwrap_or_default();
        let kind = if kind == FsEntryKind::Unknown {
            old_kind
        } else {
            kind
        };

        let entry = FsEntry {
            inode,
            path,
            kind,
            metadata,
            node_id: old_node_id.unwrap_or_default(),
        };

        let Some(first) = self
            .entries
            .iter()
            .position(|e| e.inode == entry.inode || e.path == entry.path)
        else {
            self.entries.push(entry);
            return None;
        };

        let mut idx = 0;
        self.entries.retain(|e| {
            let i = idx;
            idx += 1;
            i <= first || !(e.inode == entry.inode || e.path == entry.path)
        });

        Some(std::mem::replace(&mut self.entries[first], entry))
    }

    /// Whether `path` has any direct children. Cheap pre-check for
    /// `rmdir` (a directory with children cannot be removed).
    pub fn has_children(&self, path: &str) -> bool {
        !self.get_children_of(path).is_empty()
    }

    // --- stubs: mutation the write-side rpcs will drive ------------- ---
    // These are declared so `Store`'s write paths have a target to grow
    // into. Nothing calls them yet (every mutating `StoreApi` method
    // returns `Unimplemented` first), so they are deliberately not
    // implemented rather than guessing at conflict/offline semantics.

    /// Drop the entry for `path` from the tree, returning it.
    /// TODO: subtree handling and the "entry is open elsewhere" check.
    pub fn remove_entry(&mut self, _path: &str) -> Option<FsEntry> {
        unimplemented!("FsTree::remove_entry: needs the write-side rpcs")
    }

    /// Move `old` (and, for a directory, its subtree) to `new`.
    /// TODO: conflict detection (spec §3: no silent duplicate winner).
    pub fn rename_entry(&mut self, _old: &str, _new: &str) -> Option<FsEntry> {
        unimplemented!("FsTree::rename_entry: needs the write-side rpcs")
    }

    pub fn get_children_of(&self, path: &str) -> Vec<&FsEntry> {
        // "/" already ends with a slash; "/foo" needs one added so "/foobar" can't match
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

    fn tree() -> FsTree {
        let mut tree = FsTree { entries: vec![] };
        tree.upsert_entry("/", 1, FsEntryKind::Directory, None);
        tree.upsert_entry("/docs", 2, FsEntryKind::Directory, None);
        tree.upsert_entry("/docs/a", 3, FsEntryKind::File, None);
        tree.upsert_entry("/docs/sub", 4, FsEntryKind::Directory, None);
        tree.upsert_entry("/docs/sub/b", 5, FsEntryKind::File, None);
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
        tree.upsert_entry("/docsx", 9, FsEntryKind::File, None);
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
        assert_eq!(tree.get_inode("/docs/a"), Some(3));
        assert_eq!(tree.get_path(3), Some("/docs/a"));
        assert_eq!(tree.get_inode("/nope"), None);
        assert_eq!(tree.get_entry_by_path("/docs").map(|e| e.inode), Some(2));
    }

    #[test]
    fn upsert_reuses_inode_and_keeps_metadata_when_asked() {
        let mut tree = FsTree { entries: vec![] };
        let metadata = FsMetadata {
            size: 42,
            ..FsMetadata::default()
        };
        tree.upsert_entry("/f", 0, FsEntryKind::File, Some(metadata));
        let inode = tree.get_inode("/f").unwrap();

        // Kind/metadata not supplied => both survive the update.
        tree.upsert_entry("/f", inode, FsEntryKind::Unknown, None);
        let entry = tree.get_entry_by_path("/f").unwrap();
        assert_eq!(entry.kind, FsEntryKind::File);
        assert_eq!(entry.metadata.size, 42);
    }

    #[test]
    fn upsert_allocates_increasing_inodes() {
        let mut tree = FsTree { entries: vec![] };
        tree.upsert_entry("/a", 0, FsEntryKind::File, None);
        tree.upsert_entry("/b", 0, FsEntryKind::File, None);
        let a = tree.get_inode("/a").unwrap();
        let b = tree.get_inode("/b").unwrap();
        assert!(b > a, "expected increasing inodes, got {a} then {b}");
    }

    #[test]
    fn empty_path_keeps_the_existing_path() {
        let mut tree = FsTree { entries: vec![] };
        tree.upsert_entry("/f", 7, FsEntryKind::File, None);
        tree.upsert_entry("", 7, FsEntryKind::File, None);
        assert_eq!(tree.get_path(7), Some("/f"));
        assert_eq!(tree.entries.len(), 1);
    }

    // The mutation stubs are declared but deliberately unimplemented;
    // assert that they are reachable-but-refusing rather than silently
    // wrong, so wiring one up without implementing it fails loudly.
    #[test]
    #[should_panic(expected = "needs the write-side rpcs")]
    fn remove_entry_stub_panics() {
        tree().remove_entry("/docs");
    }

    #[test]
    #[should_panic(expected = "needs the write-side rpcs")]
    fn rename_entry_stub_panics() {
        tree().rename_entry("/docs", "/moved");
    }
}
