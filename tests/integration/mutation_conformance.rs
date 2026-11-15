//! Conformance: namespace mutation — create, remove, rename.
//!
//! Two separate questions live here, and they have different answers:
//!
//! 1. **What must work for the mount to be a filesystem?** Removing and
//!    renaming existing entries. Those tests are expected to pass.
//! 2. **What does punkfs deliberately refuse?** Creating entries through the
//!    mount. Those tests assert the refusal, so if we ever start creating
//!    entries silently, the test fails and we notice.
//!
//! Anything marked **Known gap** is a genuine conformance bug.

use std::os::unix::fs::MetadataExt;

use crate::common;

// --- creation: refused by product decision -------------------------------

#[test]
fn creating_a_new_file_is_refused_and_touches_nothing() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();

    let err = std::fs::write(mount.at("new.txt"), b"x").unwrap_err();

    // The refusal must be reported, not swallowed: a caller has to know the
    // file was not created.
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::Unsupported
        ),
        "creating a file must be refused with a documented errno, got {err:?}"
    );
    assert!(
        !server.backing_exists("new.txt"),
        "a refused create must not leave anything behind on the backing store"
    );
}

#[test]
fn mkdir_is_refused_and_touches_nothing() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();

    let err = std::fs::create_dir(mount.at("newdir")).unwrap_err();

    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
        ),
        "mkdir must be refused with a documented errno, got {err:?}"
    );
    assert!(!server.backing_exists("newdir"));
}

#[test]
fn creating_a_symlink_is_refused_and_touches_nothing() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("target.txt", b"x");

    let err = std::os::unix::fs::symlink("target.txt", mount.at("newlink")).unwrap_err();

    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
        ),
        "symlink creation must be refused with a documented errno, got {err:?}"
    );
    assert!(!server.backing_exists("newlink"));
}

#[test]
fn creating_a_hard_link_is_refused_and_touches_nothing() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("orig.txt", b"x");

    let err = std::fs::hard_link(mount.at("orig.txt"), mount.at("hard.txt")).unwrap_err();

    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
        ),
        "hard link creation must be refused with a documented errno, got {err:?}"
    );
    assert!(!server.backing_exists("hard.txt"));
}

// --- removal: must work ---------------------------------------------------

#[test]
fn unlink_removes_the_backing_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("doomed.txt", b"x");

    std::fs::remove_file(mount.at("doomed.txt")).expect("unlink must succeed");

    assert!(!server.backing_exists("doomed.txt"));
    assert_eq!(
        std::fs::metadata(mount.at("doomed.txt")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound,
        "the entry must be gone from the mount as well"
    );
}

#[test]
fn rmdir_removes_an_empty_directory() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed_dir("hollow");

    std::fs::remove_dir(mount.at("hollow")).expect("rmdir must succeed");
    assert!(!server.backing_exists("hollow"));
}

#[test]
fn rmdir_refuses_a_non_empty_directory() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("full/child.txt", b"x");

    let err = std::fs::remove_dir(mount.at("full")).unwrap_err();

    assert_eq!(
        err.kind(),
        std::io::ErrorKind::DirectoryNotEmpty,
        "rmdir on a non-empty directory must be ENOTEMPTY, got {err:?}"
    );
    assert!(
        server.backing_exists("full/child.txt"),
        "a refused rmdir must not delete anything"
    );
}

#[test]
fn unlink_on_a_directory_fails_with_eisdir() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed_dir("d");

    let err = std::fs::remove_file(mount.at("d")).unwrap_err();

    assert_eq!(
        err.kind(),
        std::io::ErrorKind::IsADirectory,
        "unlink(2) on a directory must be EISDIR, got {err:?}"
    );
    assert!(server.backing_exists("d"));
}

#[test]
fn rmdir_on_a_file_fails_with_enotdir() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("f.txt", b"x");

    let err = std::fs::remove_dir(mount.at("f.txt")).unwrap_err();

    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotADirectory,
        "rmdir(2) on a file must be ENOTDIR, got {err:?}"
    );
    assert!(server.backing_exists("f.txt"));
}

// --- rename: must work ----------------------------------------------------

#[test]
fn rename_moves_a_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("before.txt", b"payload");

    std::fs::rename(mount.at("before.txt"), mount.at("after.txt")).expect("rename must succeed");

    assert!(!server.backing_exists("before.txt"));
    assert_eq!(server.backing("after.txt"), b"payload");
}

#[test]
fn rename_moves_a_directory_with_its_contents() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("tree/inner/deep.txt", b"deep");

    std::fs::rename(mount.at("tree"), mount.at("moved")).expect("directory rename must succeed");

    assert!(!server.backing_exists("tree/inner/deep.txt"));
    assert_eq!(server.backing("moved/inner/deep.txt"), b"deep");
}

#[test]
fn rename_over_an_existing_file_replaces_it() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("src.txt", b"new");
    server.seed("dst.txt", b"old");

    std::fs::rename(mount.at("src.txt"), mount.at("dst.txt"))
        .expect("rename over an existing file must succeed");

    assert!(!server.backing_exists("src.txt"));
    assert_eq!(
        server.backing("dst.txt"),
        b"new",
        "the destination must hold the source contents"
    );
}

#[test]
fn rename_a_missing_entry_fails_with_enoent() {
    if !common::require_fuse() {
        return;
    }
    let (_server, mount) = common::mount_empty();

    let err = std::fs::rename(mount.at("ghost"), mount.at("elsewhere")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// **Known gap.** POSIX requires `rename(2)` of a directory onto a non-empty
/// directory to fail with `ENOTEMPTY`. We return EIO, and worse, the
/// operation currently *moves the source inside the destination*.
#[test]
fn rename_directory_onto_non_empty_directory_fails_with_enotempty() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed_dir("p");
    server.seed("q/keep.txt", b"keep");

    let err = std::fs::rename(mount.at("p"), mount.at("q")).unwrap_err();

    assert_eq!(
        err.kind(),
        std::io::ErrorKind::DirectoryNotEmpty,
        "CONTRACT VIOLATED\n  contract: rename(dir, non-empty dir) must fail with ENOTEMPTY\n  got:      {err:?}\n  cause:    the server maps io::ErrorKind::DirectoryNotEmpty to FailedPrecondition,\n            which the client turns into a generic Io error (EIO)"
    );
    assert!(
        server.backing_exists("p"),
        "a refused rename must leave the source in place"
    );
    assert!(
        !server.backing_exists("q/p"),
        "the source must not be nested inside the destination"
    );
}

/// **Known gap.** Renaming a directory onto an *empty* directory must
/// succeed and replace it.
#[test]
fn rename_directory_onto_empty_directory_succeeds() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("from/file.txt", b"body");
    server.seed_dir("to");

    let result = std::fs::rename(mount.at("from"), mount.at("to"));
    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: rename(dir, empty dir) must succeed\n  got:      {:?}",
        result.unwrap_err()
    );
    assert_eq!(server.backing("to/file.txt"), b"body");
}

/// **Known gap.** A rename must not disturb the identity of the entry it
/// moved. We only rewrite the cached path, so the *kernels*' cached entry and
/// our catalog disagree until a refetch, and the new path is allocated a
/// fresh inode.
#[test]
fn rename_preserves_the_inode_of_the_moved_entry() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("old.txt", b"x");
    let before = std::fs::metadata(mount.at("old.txt")).unwrap().ino();

    std::fs::rename(mount.at("old.txt"), mount.at("new.txt")).unwrap();

    let after = std::fs::metadata(mount.at("new.txt")).unwrap().ino();
    assert_eq!(
        after, before,
        "CONTRACT VIOLATED\n  contract: a rename must keep the entry's inode and uuid\n  cause:    Store::rename removes the old entry (after_mutation) before moving it,\n            so the new path is allocated a fresh local inode"
    );
}
