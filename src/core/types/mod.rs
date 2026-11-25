//! Shared filesystem types used by the cache, store, and gRPC service.
//!
//! All current attributes apply to multiple entry kinds. File-only or
//! directory-only attributes can be placed in separate modules when needed.

pub mod misc;

pub use misc::{Access, FileAttrData, FileTimes, FsEntryKind, Stat};

/// Whether the node that owns an entry is currently reachable.
/// Entries remain in the catalog when their owner is unavailable.
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
