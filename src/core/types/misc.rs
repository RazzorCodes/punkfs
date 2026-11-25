//! Attributes shared by files, directories, and other entry kinds.

/// Shared xattr operations for filesystem entries. Linux `user.*` xattrs
/// apply to regular files and directories; punkfs reserves its identity attribute.
pub(crate) mod xattr {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const GUID: &[u8] = b"user.punkfs.guid";
    const MAX_XATTR: usize = 65536;

    fn args(path: &Path, name: &[u8]) -> io::Result<(CString, CString)> {
        if name == GUID {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        if !name.starts_with(b"user.") {
            return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        Ok((path, name))
    }

    pub(crate) fn get(path: &Path, name: &[u8]) -> io::Result<Vec<u8>> {
        let (path, name) = args(path, name)?;
        let mut data = vec![0u8; MAX_XATTR];
        let n = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                data.as_mut_ptr().cast(),
                data.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        data.truncate(n as usize);
        Ok(data)
    }

    pub(crate) fn list(path: &Path) -> io::Result<Vec<u8>> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let mut data = vec![0u8; MAX_XATTR];
        let n = unsafe { libc::llistxattr(path.as_ptr(), data.as_mut_ptr().cast(), data.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        data.truncate(n as usize);
        let mut out = Vec::new();
        for name in data.split(|b| *b == 0) {
            if name.starts_with(b"user.") && name != GUID {
                out.extend_from_slice(name);
                out.push(0);
            }
        }
        Ok(out)
    }

    pub(crate) fn set(path: &Path, name: &[u8], value: &[u8], flags: i32) -> io::Result<()> {
        let (path, name) = args(path, name)?;
        if unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                flags,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn remove(path: &Path, name: &[u8]) -> io::Result<()> {
        let (path, name) = args(path, name)?;
        if unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

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
