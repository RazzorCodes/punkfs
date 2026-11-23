//! A single directory pinned before it can be covered by a FUSE mount.
//!
//! The descriptor is shared by the local filesystem and the inbound server;
//! neither adapter resolves the root through the merged mountpoint.
use crate::store::db::attributes::{Access, FileAttrData, FileTimes, FsEntryKind, Stat};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const GUID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6ba7b810_9dad_11d1_80b4_00c04fd430c8);
const GUID_XATTR: &str = "user.punkfs.guid";

/// Retains a verified descriptor while its procfs path is in use.
pub(crate) struct SafePath {
    _descriptor: File,
    path: PathBuf,
}
impl AsRef<Path> for SafePath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}
impl std::ops::Deref for SafePath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

pub(crate) struct Backing {
    root: File,
    node_id: String,
    guid_lock: std::sync::Mutex<()>,
}

impl Backing {
    pub(crate) fn open(root: &Path, node_id: &str) -> io::Result<Self> {
        let root = File::open(root)?;
        if !root.metadata()?.is_dir() {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        Ok(Self {
            root,
            node_id: node_id.into(),
            guid_lock: std::sync::Mutex::new(()),
        })
    }

    pub(crate) fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Refuse to serve a directory already covered by a FUSE filesystem. This
    /// also rejects other FUSE backings until mount identity can be verified.
    pub(crate) fn reject_fuse_mount(&self) -> io::Result<()> {
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::fstatfs(self.root.as_raw_fd(), info.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let info = unsafe { info.assume_init() };
        if info.f_type == 0x6573_5546 {
            return Err(io::Error::other(
                "backing directory is already on a FUSE filesystem",
            ));
        }
        Ok(())
    }

    fn relative(logical: &str) -> io::Result<&Path> {
        let path = Path::new(logical.trim_start_matches('/'));
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok(path)
    }

    /// Never fall back to unsafe resolution when openat2 is unavailable.
    fn open_relative(&self, path: &Path, flags: i32) -> io::Result<File> {
        use std::os::unix::ffi::OsStrExt;
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
        const RESOLVE_NO_SYMLINKS: u64 = 0x04;
        const RESOLVE_BENEATH: u64 = 0x08;
        let path = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        let raw = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.root.as_raw_fd(),
                raw.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }

    /// Pin a target without following any symlink.
    pub(crate) fn existing(&self, logical: &str) -> io::Result<SafePath> {
        let descriptor = self.open_relative(Self::relative(logical)?, libc::O_PATH)?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", descriptor.as_raw_fd()));
        Ok(SafePath {
            _descriptor: descriptor,
            path,
        })
    }

    /// Pin a name's parent. The final component is never followed by this
    /// resolver; callers must use an appropriate no-follow operation.
    pub(crate) fn entry_path(&self, logical: &str) -> io::Result<SafePath> {
        let relative = Self::relative(logical)?;
        let (parent, leaf) = match relative.file_name() {
            Some(leaf) => (relative.parent().unwrap_or(Path::new("")), leaf),
            None => (Path::new(""), std::ffi::OsStr::new(".")),
        };
        let descriptor = self.open_relative(parent, libc::O_PATH | libc::O_DIRECTORY)?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", descriptor.as_raw_fd())).join(leaf);
        Ok(SafePath {
            _descriptor: descriptor,
            path,
        })
    }

    /// Resolve a name for callers that do not support final symlinks.
    pub(crate) fn path(&self, logical: &str) -> io::Result<SafePath> {
        let path = self.entry_path(logical)?;
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                Err(io::Error::from_raw_os_error(libc::ELOOP))
            }
            Ok(_) => Ok(path),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(path),
            Err(err) => Err(err),
        }
    }

    pub(crate) fn readlink(&self, logical: &str) -> io::Result<Vec<u8>> {
        use std::os::unix::ffi::OsStrExt;
        std::fs::read_link(self.entry_path(logical)?)
            .map(|target| target.as_os_str().as_bytes().to_vec())
    }

    pub(crate) fn ensure_parents(&self, logical: &str) -> io::Result<()> {
        let path = Self::relative(logical)?;
        let mut current = PathBuf::new();
        for part in path.parent().unwrap_or(Path::new("")).components() {
            if let Component::Normal(name) = part {
                current.push(name);
                match self.open_relative(&current, libc::O_PATH | libc::O_DIRECTORY) {
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        let parent = current.parent().unwrap_or(Path::new(""));
                        let dir = self.open_relative(parent, libc::O_PATH | libc::O_DIRECTORY)?;
                        use std::os::unix::ffi::OsStrExt;
                        let name = std::ffi::CString::new(name.as_bytes())
                            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
                        if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
                            let err = io::Error::last_os_error();
                            if err.kind() != io::ErrorKind::AlreadyExists {
                                return Err(err);
                            }
                        }
                        self.open_relative(&current, libc::O_PATH | libc::O_DIRECTORY)?;
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(())
    }

    pub(crate) fn read(&self, logical: &str, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        let file = self.open_relative(Self::relative(logical)?, libc::O_RDONLY)?;
        let mut data = vec![0; size as usize];
        let count = file.read_at(&mut data, offset)?;
        data.truncate(count);
        Ok(data)
    }

    pub(crate) fn write(&self, logical: &str, offset: u64, data: &[u8]) -> io::Result<u32> {
        let file = self.open_relative(Self::relative(logical)?, libc::O_WRONLY)?;
        let mut written = 0usize;
        while written < data.len() {
            let Some(position) = offset.checked_add(written as u64) else {
                return Err(io::Error::from_raw_os_error(libc::EOVERFLOW));
            };
            match file.write_at(&data[written..], position) {
                Ok(0) => break,
                Ok(count) => written += count,
                Err(_) if written > 0 => break,
                Err(err) => return Err(err),
            }
        }
        u32::try_from(written).map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))
    }

    pub(crate) fn fsync(&self, logical: &str, datasync: bool) -> io::Result<()> {
        let file = self.open_relative(Self::relative(logical)?, libc::O_RDONLY)?;
        if datasync {
            file.sync_data()
        } else {
            file.sync_all()
        }
    }

    /// punkfs identity for a logical path, **persisted on the entry** so it
    /// survives a rename.
    ///
    /// The identity lives in the `user.punkfs.guid` xattr, which the kernel
    /// carries across `rename(2)` for free because it belongs to the inode,
    /// not the path. Minting is therefore a one-time event per file:
    ///
    /// 1. Read the xattr; if present, that is the identity.
    /// 2. Otherwise mint a fresh v4 guid and try to store it.
    /// 3. If the filesystem refuses xattrs (`EPERM`/`ENOTSUP`, e.g. a
    ///    network mount), fall back to a v5 derived from `(node, path)`.
    ///    Stable per path but *not* across renames — the pre-persistence
    ///    behaviour, kept so such a backing store still works at all.
    ///
    /// First assignment uses exclusive xattr creation so simultaneous local
    /// and inbound reads observe the same valid identity.
    fn guid_for(&self, logical: &str, real: &Path) -> String {
        let _guard = self.guid_lock.lock().unwrap();
        if let Some(guid) = read_guid_xattr(real) {
            return guid;
        }

        let minted = uuid::Uuid::new_v4().to_string();
        if set_guid_xattr(real, &minted, libc::XATTR_CREATE) {
            return minted;
        }
        // Another process may have won first assignment.
        if let Some(guid) = read_guid_xattr(real) {
            return guid;
        }
        // Replace an invalid existing xattr, but never silently replace a
        // valid identity produced by another request in this process.
        if set_guid_xattr(real, &minted, libc::XATTR_REPLACE) {
            return minted;
        }
        derived_guid(&self.node_id, logical)
    }

    /// Read a path's attributes without following a final symlink.
    pub(crate) fn attr_for(&self, logical: &str, real: &Path) -> io::Result<FileAttrData> {
        let meta = std::fs::symlink_metadata(real)?;
        let file_type = meta.file_type();

        let kind = if file_type.is_symlink() {
            FsEntryKind::Link
        } else if file_type.is_dir() {
            FsEntryKind::Directory
        } else {
            FsEntryKind::File
        };

        Ok(FileAttrData {
            guid: self.guid_for(logical, real),
            kind,
            times: FileTimes {
                accessed: meta.accessed().unwrap_or(UNIX_EPOCH),
                modified: meta.modified().unwrap_or(UNIX_EPOCH),
                changed: UNIX_EPOCH + Duration::from_secs(meta.ctime().max(0) as u64),
                created: meta.created().unwrap_or(UNIX_EPOCH),
            },
            access: Access {
                mode: meta.permissions().mode() as u16 & 0o7777,
                uid: meta.uid(),
                gid: meta.gid(),
            },
            stat: Stat {
                size: meta.len(),
                blocks: meta.blocks() as u32,
                block_size: meta.blksize() as u16,
                rdev: meta.rdev() as u16,
                nlink: meta.nlink() as u32,
                flags: 0,
            },
        })
    }
}

/// The deterministic guid used when the backing filesystem cannot store an
/// xattr.
///
/// Stable for a given `(node, path)` but **not** across renames — that is
/// the documented cost of running without xattr support.
pub(crate) fn derived_guid(node_id: &str, logical: &str) -> String {
    uuid::Uuid::new_v5(&GUID_NAMESPACE, format!("{node_id}/{logical}").as_bytes()).to_string()
}

/// Read an entry's persisted guid, if it has a valid one.
///
/// A malformed or empty xattr is treated as absent rather than an error: a
/// garbage value must not make a healthy file unreadable.
pub(crate) fn read_guid_xattr(path: &Path) -> Option<String> {
    let name = std::ffi::CString::new(GUID_XATTR).ok()?;
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;

    // lgetxattr: do not follow a final symlink, matching `attr_for`'s use of
    // `symlink_metadata`.
    let size = unsafe { libc::lgetxattr(cpath.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size <= 0 {
        return None;
    }

    let mut buf = vec![0u8; size as usize];
    let read = unsafe {
        libc::lgetxattr(
            cpath.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if read <= 0 {
        return None;
    }
    buf.truncate(read as usize);

    let text = String::from_utf8(buf).ok()?;
    let text = text.trim();
    // Only accept something that is actually a guid.
    uuid::Uuid::parse_str(text).ok()?;
    Some(text.to_owned())
}

/// Persist `guid` on an entry, reporting whether it took.
///
/// Failure is not an error: it means the backing filesystem has no xattr
/// support and the caller falls back to a derived guid.
#[cfg(test)]
pub(crate) fn write_guid_xattr(path: &Path, guid: &str) -> bool {
    set_guid_xattr(path, guid, 0)
}

fn set_guid_xattr(path: &Path, guid: &str, flags: i32) -> bool {
    let (Ok(name), Ok(cpath)) = (
        std::ffi::CString::new(GUID_XATTR),
        std::ffi::CString::new(path.as_os_str().as_encoded_bytes()),
    ) else {
        return false;
    };

    unsafe {
        libc::lsetxattr(
            cpath.as_ptr(),
            name.as_ptr(),
            guid.as_ptr().cast(),
            guid.len(),
            flags,
        ) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn symlinks_cannot_escape_the_pinned_root() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"original").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        symlink(outside.path().join("secret"), root.path().join("file-link")).unwrap();
        let backing = Backing::open(root.path(), "node").unwrap();
        for path in ["/escape/secret", "/file-link"] {
            assert_eq!(
                backing.read(path, 0, 64).unwrap_err().raw_os_error(),
                Some(libc::ELOOP)
            );
            assert_eq!(
                backing
                    .write(path, 0, b"changed")
                    .unwrap_err()
                    .raw_os_error(),
                Some(libc::ELOOP)
            );
            assert!(backing.path(path).is_err());
        }
        assert!(backing.ensure_parents("/escape/new").is_err());
        assert_eq!(
            std::fs::read(outside.path().join("secret")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn concurrent_first_observations_share_one_guid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), b"data").unwrap();
        let backings = [
            Arc::new(Backing::open(dir.path(), "node").unwrap()),
            Arc::new(Backing::open(dir.path(), "node").unwrap()),
        ];
        let barrier = Arc::new(Barrier::new(16));
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let backing = backings[i % 2].clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    backing
                        .attr_for("/file", &backing.path("/file").unwrap())
                        .unwrap()
                        .guid
                })
            })
            .collect();
        let guids: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(guids.iter().all(|guid| guid == &guids[0]));
    }
}
