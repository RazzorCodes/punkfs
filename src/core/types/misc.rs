//! Attributes shared by files, directories, and other entry kinds.

pub(crate) mod xattr;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    pub fn from_secs(atime: u64, mtime: u64, ctime: u64, crtime: u64) -> Self {
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
    /// Permission bits, e.g. `0o755`. Zero is a valid mode.
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
    pub rdev: u32,
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
    Fifo,
    Socket,
    BlockDevice,
    CharDevice,
}

/// Stat-shaped metadata for any entry kind, despite the `FileAttrData` name.
/// It mirrors the proto `FileAttr` one-to-one so conversions are mechanical.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileAttrData {
    /// punkfs identity; stable across renames once the catalog owns it.
    pub guid: String,
    pub kind: FsEntryKind,
    pub times: FileTimes,
    pub access: Access,
    pub stat: Stat,
}

#[cfg(test)]
mod tests {
    use super::*;

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
