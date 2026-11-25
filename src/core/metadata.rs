//! Shared metadata types used by the cache, persistent catalog, and routing.

use super::types::{FileAttrData, FsEntryKind};

/// Whether the node that owns an entry is currently reachable.
///
/// Entries remain in the catalog when their owner is unavailable; only
/// operations that need the owner's backing data fail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Availability {
    #[default]
    Available,
    /// Owning node has not answered recently; content operations will fail.
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
