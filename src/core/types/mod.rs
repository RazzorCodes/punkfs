//! Shared filesystem types used by the cache, store, and gRPC service.
//!
//! All current attributes apply to multiple entry kinds. File-only or
//! directory-only attributes can be placed in separate modules when needed.

pub mod misc;

pub use misc::{Access, FileAttrData, FileTimes, FsEntryKind, Stat};
