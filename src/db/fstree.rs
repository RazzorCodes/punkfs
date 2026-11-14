pub struct FsNode {
    pub node_id: String,
}

pub struct FsNodeMetadata {
    pub parent_node: String,
    pub uuid: String,
}

/// The stat-shaped metadata carried by every entry. All fields default
/// to zero, so `FsMetadata::default()` is the "empty" metadata.
#[derive(Default, Clone, Copy, Debug)]
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

#[derive(Clone, Debug)]
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
