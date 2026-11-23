//! Shared syscall-level contracts for mounted and blackbox filesystems.
//!
//! Linux-specific contracts probe xattr and ACL support on a local temporary
//! filesystem. The renameat2 tests require a supporting Linux kernel and
//! backing filesystem. Missing FUSE is handled by the caller, not here.
#![cfg(target_os = "linux")]
#![allow(dead_code)]

use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::sync::{Arc, Barrier};

fn cpath(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap()
}
fn last_error(context: &str) -> ! {
    panic!("{context}: {}", std::io::Error::last_os_error())
}

/// Caller supplies an already-existing file on a remote owner.
pub fn truncate(root: &Path) {
    let path = root.join("data");
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .expect("O_TRUNC must succeed");
    assert_eq!(fs::read(&path).unwrap(), b"");
    fs::write(&path, b"abcdef").expect("write after truncate");
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(2).expect("ftruncate must succeed");
    assert_eq!(fs::read(&path).unwrap(), b"ab");
}

pub fn mode(root: &Path) {
    let path = root.join("data");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod must succeed");
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
}

pub fn timestamps(root: &Path) {
    let path = cpath(&root.join("data"));
    let times = [
        libc::timespec {
            tv_sec: 1_000_000,
            tv_nsec: 123_456_789,
        },
        libc::timespec {
            tv_sec: 1_000_001,
            tv_nsec: 987_654_321,
        },
    ];
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
    if rc != 0 {
        last_error("utimensat must succeed");
    }
    let meta = fs::metadata(root.join("data")).unwrap();
    assert_eq!(
        (meta.mtime(), meta.mtime_nsec()),
        (times[1].tv_sec, times[1].tv_nsec),
        "mtime must retain nanoseconds"
    );
}

pub fn open_handle_unlink(root: &Path) {
    let path = root.join("data");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    fs::remove_file(&path).expect("unlink existing file");
    let mut content = String::new();
    file.read_to_string(&mut content)
        .expect("open handle must remain readable after unlink");
    assert_eq!(content, "abcdef");
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(b"Z")
        .expect("open handle must remain writable after unlink");
    file.seek(SeekFrom::Start(0)).unwrap();
    content.clear();
    file.read_to_string(&mut content).unwrap();
    assert_eq!(content, "Zbcdef");
    assert!(!path.exists());
}

pub fn open_handle_rename(root: &Path) {
    let path = root.join("data");
    let mut file = File::open(&path).unwrap();
    let before = file.metadata().unwrap().ino();
    fs::rename(&path, root.join("renamed")).expect("rename an open file");
    let mut content = String::new();
    file.read_to_string(&mut content)
        .expect("open handle remains readable after rename");
    assert_eq!(content, "abcdef");
    assert_eq!(
        fs::metadata(root.join("renamed")).unwrap().ino(),
        before,
        "rename retains inode"
    );
}

pub fn uid_gid_match(root: &Path, uid: u32, gid: u32) {
    let metadata = fs::metadata(root.join("data")).unwrap();
    assert_eq!(
        (metadata.uid(), metadata.gid()),
        (uid, gid),
        "reported owner must match backing owner, including zero-valued IDs"
    );
}

pub fn permissions(root: &Path) {
    let metadata = fs::metadata(root.join("data")).unwrap();
    assert_eq!(
        metadata.mode() & 0o7777,
        0,
        "mode 0000 is meaningful and must not become 0777"
    );
}

pub fn access_executable(root: &Path) {
    // Unlike read/write checks, even root requires at least one execute bit
    // to succeed for a regular file.
    let path = cpath(&root.join("data"));
    let rc = unsafe { libc::access(path.as_ptr(), libc::X_OK) };
    assert_eq!(
        rc, -1,
        "access(X_OK) must deny a regular file with no execute bits"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EACCES)
    );
}

pub fn ownership(root: &Path) {
    let path = root.join("data");
    let before = fs::metadata(&path).unwrap();
    // POSIX permits an owner to retain its own uid and current group; this
    // avoids requiring root or requesting arbitrary ownership changes.
    let c = cpath(&path);
    let rc = unsafe { libc::chown(c.as_ptr(), before.uid(), before.gid()) };
    if rc != 0 {
        last_error("chown to existing uid and gid must succeed");
    }
    let after = fs::metadata(&path).unwrap();
    assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
}

pub fn access_checks(root: &Path) {
    // access(2) uses real credentials. Root is intentionally excluded: root
    // may have CAP_DAC_OVERRIDE, so a mode-000 file is not a portable denial.
    if unsafe { libc::getuid() } == 0 {
        eprintln!("skipping access denial contract for root credentials");
        return;
    }
    let path = cpath(&root.join("data"));
    let rc = unsafe { libc::access(path.as_ptr(), libc::R_OK) };
    assert_eq!(rc, -1, "access(R_OK) must deny a mode-000 file");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EACCES)
    );
}

pub fn hard_link(root: &Path) {
    let source = root.join("data");
    let link = root.join("hard");
    fs::hard_link(&source, &link).expect("hard link on the same mount must succeed");
    let a = fs::metadata(&source).unwrap();
    let b = fs::metadata(&link).unwrap();
    assert_eq!(a.ino(), b.ino(), "hard links must have the same inode");
    assert!(a.nlink() >= 2);
    fs::remove_file(source).unwrap();
    assert_eq!(fs::read(link).unwrap(), b"abcdef");
}

pub fn local_hard_link(root: &Path) {
    fs::write(root.join("local-file"), b"local").unwrap();
    fs::hard_link(root.join("local-file"), root.join("local-hard"))
        .expect("local hard link must succeed");
    assert_eq!(
        fs::metadata(root.join("local-file")).unwrap().ino(),
        fs::metadata(root.join("local-hard")).unwrap().ino()
    );
}

pub fn seeded_xattr_list(root: &Path) {
    let path = cpath(&root.join("data"));
    let mut names = [0u8; 512];
    let n = unsafe { libc::listxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if n < 0 {
        last_error("list seeded user xattr");
    }
    assert!(
        names[..n as usize]
            .split(|byte| *byte == 0)
            .any(|name| name == b"user.punkfs_seed")
    );
}

pub fn seeded_xattr_remove(root: &Path) {
    let path = cpath(&root.join("data"));
    if unsafe { libc::removexattr(path.as_ptr(), c"user.punkfs_seed".as_ptr()) } != 0 {
        last_error("remove seeded user xattr");
    }
    let mut value = [0u8; 16];
    let n = unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"user.punkfs_seed".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    assert_eq!(n, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ENODATA)
    );
}

pub fn seeded_xattr(root: &Path) {
    let path = cpath(&root.join("data"));
    let mut value = [0u8; 16];
    let n = unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"user.punkfs_seed".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if n < 0 {
        last_error("get seeded user xattr");
    }
    assert_eq!(&value[..n as usize], b"seed");
    let mut names = [0u8; 512];
    let n = unsafe { libc::listxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if n < 0 {
        last_error("list seeded user xattr");
    }
    assert!(
        names[..n as usize]
            .split(|byte| *byte == 0)
            .any(|name| name == b"user.punkfs_seed")
    );
    if unsafe { libc::removexattr(path.as_ptr(), c"user.punkfs_seed".as_ptr()) } != 0 {
        last_error("remove seeded user xattr");
    }
    let n = unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"user.punkfs_seed".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    assert_eq!(n, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ENODATA)
    );
}

pub fn seed_xattr(path: &Path) -> std::io::Result<()> {
    let path = cpath(path);
    let rc = unsafe {
        libc::setxattr(
            path.as_ptr(),
            c"user.punkfs_seed".as_ptr(),
            b"seed".as_ptr().cast(),
            4,
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub fn fifo(root: &Path) {
    let path = root.join("fifo");
    let c = cpath(&path);
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        last_error("mkfifo must create a FIFO");
    }
    use std::os::unix::fs::FileTypeExt;
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_fifo());
    fs::remove_file(path).unwrap();
}

fn set_xattr(path: &Path, value: &[u8]) -> std::io::Result<()> {
    let path = cpath(path);
    let name = c"user.punkfs_test";
    let rc = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub fn xattr(root: &Path) {
    let path = root.join("data");
    let probe = tempfile::tempdir().unwrap();
    let p = probe.path().join("file");
    fs::write(&p, b"").unwrap();
    if let Err(error) = set_xattr(&p, b"value") {
        if matches!(
            error.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::EPERM)
        ) {
            eprintln!(
                "skipping xattr contract: local test filesystem does not support user xattrs ({error})"
            );
            return;
        }
        panic!("xattr capability probe failed: {error}");
    }
    set_xattr(&path, b"value").expect("setxattr must work when underlying platform supports it");
    let mut buf = [0u8; 16];
    let path = cpath(&path);
    let n = unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"user.punkfs_test".as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n < 0 {
        last_error("getxattr");
    }
    assert_eq!(&buf[..n as usize], b"value");
    if unsafe { libc::removexattr(path.as_ptr(), c"user.punkfs_test".as_ptr()) } != 0 {
        last_error("removexattr");
    }
}

pub fn xattr_list(root: &Path) {
    let path = root.join("data");
    let probe = tempfile::tempdir().unwrap();
    let local = probe.path().join("file");
    fs::write(&local, b"").unwrap();
    if let Err(error) = set_xattr(&local, b"value") {
        if matches!(
            error.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::EPERM)
        ) {
            eprintln!("skipping xattr list contract: local filesystem unsupported ({error})");
            return;
        }
        panic!("xattr capability probe failed: {error}");
    }
    set_xattr(&path, b"value").expect("set user attribute before listing");
    let c = cpath(&path);
    let mut buf = [0u8; 512];
    let n = unsafe { libc::listxattr(c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        last_error("listxattr");
    }
    assert!(
        buf[..n as usize]
            .split(|byte| *byte == 0)
            .any(|name| name == b"user.punkfs_test")
    );
}

pub fn acl_roundtrip(root: &Path) {
    // Use the kernel's POSIX ACL xattr format. Probe the test filesystem so
    // unsupported ACLs are not confused with a punkfs failure.
    let probe = tempfile::tempdir().unwrap();
    let path = probe.path().join("file");
    fs::write(&path, b"").unwrap();
    let user = unsafe { libc::getuid() };
    let mut acl = Vec::new();
    acl.extend_from_slice(&2u32.to_le_bytes());
    for (tag, perm, id) in [
        (1u16, 6u16, u32::MAX),
        (2, 4, user),
        (4, 0, u32::MAX),
        (16, 4, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend_from_slice(&tag.to_le_bytes());
        acl.extend_from_slice(&perm.to_le_bytes());
        acl.extend_from_slice(&id.to_le_bytes());
    }
    let set = |path: &Path| {
        let path = cpath(path);
        let rc = unsafe {
            libc::setxattr(
                path.as_ptr(),
                c"system.posix_acl_access".as_ptr(),
                acl.as_ptr().cast(),
                acl.len(),
                0,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };
    if let Err(err) = set(&path) {
        if matches!(
            err.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::EPERM)
        ) {
            eprintln!(
                "skipping POSIX ACL contract: local test filesystem does not support ACLs ({err})"
            );
            return;
        }
        panic!("ACL capability probe failed: {err}");
    }
    let path = root.join("data");
    set(&path).expect("POSIX ACL must be settable");
    let mut actual = vec![0u8; 256];
    let path = cpath(&path);
    let n = unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"system.posix_acl_access".as_ptr(),
            actual.as_mut_ptr().cast(),
            actual.len(),
        )
    };
    if n < 0 {
        last_error("get POSIX ACL");
    }
    assert_eq!(&actual[..n as usize], acl.as_slice());
}

pub fn byte_filename(root: &Path) {
    let name = OsString::from_vec(vec![b'n', 0xff]);
    let path = root.join(&name);
    fs::write(&path, b"bytes").expect("Unix filename bytes need not be UTF-8");
    assert_eq!(fs::read(&path).unwrap(), b"bytes");
    assert!(
        fs::read_dir(root)
            .unwrap()
            .any(|entry| entry.unwrap().file_name() == name)
    );
}

pub fn create_file_and_directory(root: &Path) {
    fs::write(root.join("created"), b"created").expect("create regular file");
    fs::create_dir(root.join("directory")).expect("create directory");
    assert_eq!(fs::read(root.join("created")).unwrap(), b"created");
    assert!(fs::metadata(root.join("directory")).unwrap().is_dir());
}

pub fn symlink_semantics(root: &Path) {
    let link = root.join("link");
    symlink("data", &link).expect("create symlink");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("data"));
    assert_eq!(fs::read(&link).unwrap(), b"abcdef");
    fs::remove_file(&link).unwrap();
    assert_eq!(fs::read(root.join("data")).unwrap(), b"abcdef");
}

pub fn concurrent_append(root: &Path) {
    let path = root.join("data");
    let barrier = Arc::new(Barrier::new(3));
    let threads: Vec<_> = [b'A', b'B']
        .into_iter()
        .map(|byte| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut file = OpenOptions::new().append(true).open(path).unwrap();
                barrier.wait();
                for _ in 0..64 {
                    file.write_all(&[byte; 16]).expect("append record");
                }
            })
        })
        .collect();
    barrier.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..6], b"abcdef");
    assert_eq!(
        bytes.len(),
        6 + 2 * 64 * 16,
        "concurrent append must not lose writes"
    );
    for record in bytes[6..].chunks_exact(16) {
        assert!(record.iter().all(|b| *b == record[0]) && [b'A', b'B'].contains(&record[0]));
    }
}

pub fn advisory_lock(root: &Path) {
    // flock locks are per open-file description, so two separately opened
    // descriptors must contend even inside one process.
    let first = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("data"))
        .unwrap();
    let second = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("data"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "first lock"
    );
    let result = unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    let err = if result == -1 {
        std::io::Error::last_os_error().raw_os_error()
    } else {
        None
    };
    assert_eq!(
        unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_UN) },
        0,
        "unlock"
    );
    assert_eq!(
        result, -1,
        "a second independent descriptor must not acquire an exclusive lock"
    );
    assert_eq!(err, Some(libc::EWOULDBLOCK));
}

pub fn rename_noreplace(root: &Path) {
    let old = cpath(&root.join("data"));
    let new = cpath(&root.join("other"));
    assert_eq!(fs::read(root.join("other")).unwrap(), b"other");
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    assert_eq!(
        rc, -1,
        "RENAME_NOREPLACE must refuse an existing destination"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EEXIST)
    );
    assert_eq!(fs::read(root.join("data")).unwrap(), b"abcdef");
    assert_eq!(fs::read(root.join("other")).unwrap(), b"other");
}

pub fn rename_exchange(root: &Path) {
    // Two existing files on the same underlying owner can be exchanged.
    // The caller pre-seeds `other` alongside `data` on that owner.
    assert_eq!(fs::read(root.join("other")).unwrap(), b"other");
    let old = cpath(&root.join("data"));
    let new = cpath(&root.join("other"));
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if rc != 0 {
        last_error("RENAME_EXCHANGE must swap two files");
    }
    assert_eq!(fs::read(root.join("data")).unwrap(), b"other");
    assert_eq!(fs::read(root.join("other")).unwrap(), b"abcdef");
}

pub fn concurrent_create_exclusive(root: &Path) {
    let path = root.join("exclusive");
    let barrier = Arc::new(Barrier::new(3));
    let threads: Vec<_> = [b'A', b'B']
        .into_iter()
        .map(|byte| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                match OpenOptions::new().write(true).create_new(true).open(path) {
                    Ok(mut file) => {
                        file.write_all(&[byte]).unwrap();
                        Ok(byte)
                    }
                    Err(err) => Err(err.raw_os_error()),
                }
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "one exclusive create must win"
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(Some(libc::EEXIST))))
            .count(),
        1,
        "the other create must report EEXIST"
    );
    assert_eq!(
        fs::read(path).unwrap(),
        [results.into_iter().find_map(Result::ok).unwrap()]
    );
}

pub fn fsync_data(root: &Path) {
    let path = root.join("data");
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all(b"synced").unwrap();
    file.sync_data().expect("fdatasync must succeed");
    file.sync_all().expect("fsync must succeed");
    assert_eq!(fs::read(path).unwrap(), b"synced");
}

pub fn mmap_read(root: &Path) {
    let file = File::open(root.join("data")).unwrap();
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            6,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(
        mapping,
        libc::MAP_FAILED,
        "mmap must succeed: {}",
        std::io::Error::last_os_error()
    );
    let bytes = unsafe { std::slice::from_raw_parts(mapping.cast::<u8>(), 6) };
    assert_eq!(bytes, b"abcdef");
    assert_eq!(unsafe { libc::munmap(mapping, 6) }, 0);
}

pub fn open_handle_replaced_destination(root: &Path) {
    let mut old = File::open(root.join("other")).unwrap();
    fs::rename(root.join("data"), root.join("other")).expect("replace an existing same-owner file");
    let mut bytes = Vec::new();
    old.read_to_end(&mut bytes)
        .expect("open descriptor must continue to reference replaced inode");
    assert_eq!(
        bytes, b"other",
        "old handle must not silently switch to the replacement"
    );
    assert_eq!(fs::read(root.join("other")).unwrap(), b"abcdef");
}

pub fn rename_nonempty(root: &Path) {
    fs::create_dir(root.join("source")).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    fs::write(root.join("dest/keep"), b"keep").unwrap();
    let error = fs::rename(root.join("source"), root.join("dest")).unwrap_err();
    assert!(
        matches!(
            error.raw_os_error(),
            Some(libc::ENOTEMPTY) | Some(libc::EEXIST)
        ),
        "nonempty directory rename must return ENOTEMPTY/EEXIST, got {error}"
    );
    assert!(root.join("source").is_dir());
    assert_eq!(fs::read(root.join("dest/keep")).unwrap(), b"keep");
}
