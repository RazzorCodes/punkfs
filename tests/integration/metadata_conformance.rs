//! Conformance: metadata, statfs, and symlinks.
//!
//! `stat` correctness matters more than it looks: `ls -l`, `tar`, `rsync`,
//! `find -size`, and every installer read these fields.

use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;

use crate::common;

#[test]
fn stat_reports_size_kind_owner_and_mode_from_the_backing_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("sized.txt", &vec![b'x'; 1234]);
    std::fs::set_permissions(
        server.root().join("sized.txt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();

    let fuse = std::fs::metadata(mount.at("sized.txt")).unwrap();
    let disk = std::fs::metadata(server.root().join("sized.txt")).unwrap();

    assert_eq!(fuse.len(), 1234, "st_size must match the backing file");
    assert_eq!(fuse.is_file(), disk.is_file());
    assert_eq!(
        fuse.permissions().mode() & 0o7777,
        disk.permissions().mode() & 0o7777,
        "st_mode must match the backing file"
    );
    assert_eq!(fuse.uid(), disk.uid(), "st_uid must match");
    assert_eq!(fuse.gid(), disk.gid(), "st_gid must match");
    assert_eq!(fuse.nlink(), disk.nlink(), "st_nlink must match");
}

#[test]
fn stat_reports_mtime_from_the_backing_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("m.txt", b"x");

    let fuse = std::fs::metadata(mount.at("m.txt")).unwrap();
    let disk = std::fs::metadata(server.root().join("m.txt")).unwrap();

    let diff = fuse
        .modified()
        .unwrap()
        .duration_since(disk.modified().unwrap())
        .or_else(|e| Ok::<_, std::time::SystemTimeError>(e.duration()))
        .unwrap();
    assert!(
        diff.as_secs() <= 1,
        "st_mtime must match the backing file (diff {diff:?})"
    );
}

#[test]
fn directory_nlink_counts_subdirectories() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed_dir("two-subdirs/a");
    server.seed_dir("two-subdirs/b");
    server.seed_dir("empty");

    // POSIX: a directory's link count is 2 + the number of subdirectories.
    assert_eq!(
        std::fs::metadata(mount.at("two-subdirs")).unwrap().nlink(),
        4,
        "a directory with two subdirectories must report nlink 4"
    );
    assert_eq!(
        std::fs::metadata(mount.at("empty")).unwrap().nlink(),
        2,
        "an empty directory must report nlink 2"
    );
}

#[test]
fn symlinks_are_reported_as_symlinks_by_lstat() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("target.txt", b"body");
    std::os::unix::fs::symlink("target.txt", server.root().join("link")).unwrap();

    let meta = std::fs::symlink_metadata(mount.at("link")).unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "lstat must report a symlink, not a regular file"
    );
}

/// The returned target is the link's stored target, not its resolved path.
#[test]
fn readlink_returns_the_symlink_target() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("target.txt", b"body");
    std::os::unix::fs::symlink("target.txt", server.root().join("link")).unwrap();

    let result = std::fs::read_link(mount.at("link"));

    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: readlink(2) on a symlink must return its target\n  got:      {:?}\n  expected: the stored link target",
        result.unwrap_err()
    );
    assert_eq!(result.unwrap(), std::path::Path::new("target.txt"));
}

/// Following a link must resolve its target through the mounted namespace.
#[test]
fn opening_a_symlink_reads_through_to_the_target() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("target.txt", b"body");
    std::os::unix::fs::symlink("target.txt", server.root().join("link")).unwrap();

    let result = std::fs::read(mount.at("link"));

    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: opening a symlink must read the target's contents\n  got:      {:?}",
        result.unwrap_err()
    );
    assert_eq!(result.unwrap(), b"body");
}

#[test]
fn statfs_matches_the_backing_filesystem() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();

    let fuse = statvfs(mount.path());
    let disk = statvfs(server.root());

    assert_eq!(
        fuse.block_size, disk.block_size,
        "f_bsize must be the backing filesystem's block size"
    );
    assert_eq!(
        fuse.name_length, disk.name_length,
        "f_namemax must be the backing filesystem's limit"
    );
    assert!(
        fuse.blocks > 0,
        "f_blocks must be positive so `df` has something to show"
    );
}

/// **Known gap.** `chmod` needs `setattr`, which is refused.
#[test]
fn chmod_changes_the_mode_on_the_backing_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("c.txt", b"x");
    std::fs::set_permissions(
        server.root().join("c.txt"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    let result =
        std::fs::set_permissions(mount.at("c.txt"), std::fs::Permissions::from_mode(0o600));

    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: chmod(2) must succeed and update the backing mode\n  got:      {:?}\n  cause:    setattr is refused with ENOSYS",
        result.unwrap_err()
    );
    let mode = std::fs::metadata(server.root().join("c.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the backing mode must have changed");
}

/// **Known gap.** `utimensat(2)` needs `setattr`.
#[test]
fn utime_sets_the_backing_mtime() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("u.txt", b"x");

    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    let result = boss_utime(&mount.at("u.txt"), when);

    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: utimensat(2) must set the file's mtime\n  got:      {:?}\n  cause:    setattr is refused with ENOSYS",
        result.unwrap_err()
    );

    // And the change must be visible in the backing store.
    let mtime = std::fs::metadata(server.root().join("u.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        1_000_000
    );
}

/// `utimensat(2)` directly, so the test sees the real errno rather than a
/// shell tool's summary.
fn boss_utime(path: &std::path::Path, when: std::time::SystemTime) -> std::io::Result<()> {
    let secs = when
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let times = [
        libc::timespec {
            tv_sec: secs as libc::time_t,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: secs as libc::time_t,
            tv_nsec: 0,
        },
    ];
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::other("NUL in path"))?;

    // SAFETY: valid C string, two-element timespec array as utimensat wants.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[test]
fn read_only_mount_permissions_are_reported_for_real_files() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("ro.txt", b"x");
    std::fs::set_permissions(
        server.root().join("ro.txt"),
        std::fs::Permissions::from_mode(0o400),
    )
    .unwrap();

    let mode = std::fs::metadata(mount.at("ro.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o400,
        "a read-only backing file must report mode 0400 through the mount"
    );
}

struct VfsStats {
    block_size: u64,
    blocks: u64,
    name_length: u64,
}

/// `statvfs(3)` through libc, because there is no std wrapper.
fn statvfs(path: &std::path::Path) -> VfsStats {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid C string and `buf` is a zeroed out-param.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    assert_eq!(rc, 0, "statvfs failed for {}", path.display());
    VfsStats {
        block_size: buf.f_bsize as u64,
        blocks: buf.f_blocks,
        name_length: buf.f_namemax,
    }
}

/// **Known gap.** We never implement `access(2)`, so the fuser default answers
/// "allowed" for everything. A caller checking writability of a mode-0400 file
/// is told yes, then the write fails — worse than a plain refusal, because a
/// program can make decisions on a false answer.
#[test]
fn access_reports_a_read_only_file_as_not_writable() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("ro.txt", b"x");
    std::fs::set_permissions(
        server.root().join("ro.txt"),
        std::fs::Permissions::from_mode(0o400),
    )
    .unwrap();

    // `std` has no access(2) wrapper; call it through libc.
    let writable = access(&mount.at("ro.txt"), libc::W_OK);

    assert!(
        !writable,
        "CONTRACT VIOLATED\n  contract: access(W_OK) must be denied for a mode-0400 file\n  got:      allowed\n  cause:    access(2) is not implemented, so the trait default answers success"
    );
}

#[test]
fn access_allows_a_read_only_file_to_be_read() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("ro.txt", b"x");
    std::fs::set_permissions(
        server.root().join("ro.txt"),
        std::fs::Permissions::from_mode(0o400),
    )
    .unwrap();

    assert!(
        access(&mount.at("ro.txt"), libc::R_OK),
        "a mode-0400 file must be readable, so access(R_OK) must be allowed"
    );
}

#[test]
fn access_denies_a_missing_path() {
    if !common::require_fuse() {
        return;
    }
    let (_server, mount) = common::mount_empty();

    assert!(
        !access(&mount.at("nope"), libc::F_OK),
        "access(F_OK) on a nonexistent path must be denied"
    );
}

/// `access(2)` for `mode`.
fn access(path: &std::path::Path, mode: i32) -> bool {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: valid C string.
    unsafe { libc::access(c_path.as_ptr(), mode) == 0 }
}
