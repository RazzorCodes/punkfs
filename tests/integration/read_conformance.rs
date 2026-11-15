//! Conformance: reading a mounted punkfs.
//!
//! These assert what any POSIX filesystem must do for ordinary read
//! workloads (`cat`, `less`, `grep`, media players, `tar`). They are written
//! against the contract, not against current behaviour: a failure here is a
//! real gap, and the panic message states expected vs actual.
//!
//! A test whose comment says **KNOWN GAP** is expected to fail today. Do not
//! "fix" such a test by relaxing it — fix the filesystem.

use std::io::{Read, Seek, SeekFrom};

use crate::common;

#[test]
fn read_directory_lists_exactly_the_backing_children() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("alpha.txt", b"a");
    server.seed("beta.txt", b"b");
    server.seed_dir("gamma");

    let mut names: Vec<String> = std::fs::read_dir(mount.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    assert_eq!(
        names,
        vec!["alpha.txt", "beta.txt", "gamma"],
        "readdir must report exactly the backing entries, no more and no fewer"
    );
}

#[test]
fn readdir_is_stable_and_lists_every_entry_in_a_large_directory() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    let expected: Vec<String> = (0..300).map(|i| format!("file{i:03}.txt")).collect();
    for name in &expected {
        server.seed(name, b"x");
    }

    // A directory bigger than one getdents buffer: the kernel calls readdir
    // repeatedly, and every entry must reach the caller exactly once.
    let mut names: Vec<String> = std::fs::read_dir(mount.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names, expected,
        "a 300-entry directory must be listed completely and without duplicates"
    );

    let mut again: Vec<String> = std::fs::read_dir(mount.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    again.sort();
    assert_eq!(again, names, "repeated readdir must be stable");
}

#[test]
fn dot_and_dotdot_resolve_to_real_inodes() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed_dir("parent/child");
    let parent = mount.at("parent");
    let child = mount.at("parent/child");

    use std::os::unix::fs::MetadataExt;
    let parent_ino = std::fs::metadata(&parent).unwrap().ino();
    let dot_ino = std::fs::metadata(parent.join(".")).unwrap().ino();
    let dotdot_ino = std::fs::metadata(child.join("..")).unwrap().ino();

    assert_eq!(dot_ino, parent_ino, "`.` must be the directory itself");
    assert_eq!(
        dotdot_ino, parent_ino,
        "`..` must be the containing directory"
    );
}

#[test]
fn root_dotdot_resolves_to_root() {
    if !common::require_fuse() {
        return;
    }
    let (_server, mount) = common::mount_empty();
    use std::os::unix::fs::MetadataExt;
    let root_ino = std::fs::metadata(mount.path()).unwrap().ino();
    let dotdot_ino = std::fs::metadata(mount.at("..")).unwrap().ino();
    assert_eq!(dotdot_ino, root_ino, "/.. must be /");
}

#[test]
fn read_returns_exact_backing_bytes() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    let payload: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
    server.seed("binary.bin", &payload);

    let got = std::fs::read(mount.at("binary.bin")).unwrap();
    assert_eq!(
        got, payload,
        "read must return the backing bytes unchanged (binary-safe)"
    );
}

#[test]
fn empty_file_reads_as_empty() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("empty.txt", b"");

    assert_eq!(std::fs::metadata(mount.at("empty.txt")).unwrap().len(), 0);
    assert_eq!(std::fs::read(mount.at("empty.txt")).unwrap(), Vec::<u8>::new());
}

#[test]
fn reads_at_arbitrary_offsets_return_the_right_window() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    let payload: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
    server.seed("window.bin", &payload);

    let mut file = std::fs::File::open(mount.at("window.bin")).unwrap();
    for offset in [0u64, 1, 511, 4096, 65_535] {
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut buf = vec![0u8; 16];
        let n = file.read(&mut buf).unwrap();
        let want = &payload[offset as usize..(offset as usize + 16).min(payload.len())];
        assert_eq!(
            &buf[..n],
            want,
            "read at offset {offset} returned the wrong window"
        );
    }
}

#[test]
fn read_past_eof_is_a_short_read_not_an_error() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("short.txt", b"abc");

    let mut file = std::fs::File::open(mount.at("short.txt")).unwrap();
    file.seek(SeekFrom::Start(100)).unwrap();
    let mut buf = [0u8; 8];
    let n = file.read(&mut buf).unwrap();
    assert_eq!(n, 0, "reading past EOF must return 0 bytes, not an error");
}

/// POSIX allows `open(dir, O_RDONLY)`; it is `read(2)` on the resulting fd
/// that fails with EISDIR. This is the half that must succeed.
#[test]
fn open_a_directory_read_only_succeeds() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed_dir("d");

    std::fs::File::open(mount.at("d"))
        .expect("open(dir, O_RDONLY) must succeed");
}

/// The other half: opening a directory for writing is EISDIR.
#[test]
fn open_a_directory_for_writing_fails_with_eisdir() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed_dir("d");

    let err = std::fs::OpenOptions::new()
        .write(true)
        .open(mount.at("d"))
        .unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::IsADirectory,
        "open(dir, O_WRONLY) must be EISDIR, got {err:?}"
    );
}

/// And `read(2)` on a directory fd is EISDIR, not a short read.
#[test]
fn read_on_a_directory_descriptor_fails_with_eisdir() {
    use std::io::Read;

    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed_dir("d");

    let mut file = std::fs::File::open(mount.at("d")).expect("open(dir, O_RDONLY)");
    let mut buf = [0u8; 16];
    let err = file.read(&mut buf).unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::IsADirectory,
        "read(2) on a directory must be EISDIR, got {err:?}"
    );
}

#[test]
fn readdir_on_a_regular_file_fails_with_enotdir() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("f.txt", b"x");

    let err = std::fs::read_dir(mount.at("f.txt")).unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotADirectory,
        "opendir on a regular file must be ENOTDIR, got {err:?}"
    );
}

#[test]
fn missing_entries_report_enoent() {
    if !common::require_fuse() {
        return;
    }
    let (_server, mount) = common::mount_empty();

    let err = std::fs::metadata(mount.at("nope")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    let err = std::fs::File::open(mount.at("nope")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn unicode_filenames_round_trip() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("ünïcode-🎉.txt", b"unicode");

    let got = std::fs::read(mount.at("ünïcode-🎉.txt")).unwrap();
    assert_eq!(got, b"unicode", "non-ASCII names must survive the round trip");

    let listed: Vec<String> = std::fs::read_dir(mount.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        listed.iter().any(|n| n.starts_with("ünïcode")),
        "unicode name missing from readdir: {listed:?}"
    );
}

#[test]
fn nested_directories_are_walkable_to_full_depth() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("a/b/c/d/e/deep.txt", b"deep");

    assert_eq!(
        std::fs::read_to_string(mount.at("a/b/c/d/e/deep.txt")).unwrap(),
        "deep"
    );
}

#[test]
fn a_multi_megabyte_file_reads_back_identical() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    let payload: Vec<u8> = (0..5_000_000).map(|i| (i % 251) as u8).collect();
    server.seed("big.bin", &payload);

    let got = std::fs::read(mount.at("big.bin")).unwrap();
    assert_eq!(
        got.len(),
        payload.len(),
        "size must match the backing file"
    );
    assert!(
        got == payload,
        "a 5 MB read must be byte-identical to the backing file"
    );
}

#[test]
fn find_walks_every_file_in_the_tree() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("one/a.txt", b"1");
    server.seed("one/two/b.txt", b"2");
    server.seed("one/two/three/c.txt", b"3");

    let mut found: Vec<String> = std::fs::read_dir(mount.at("one"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    assert_eq!(found, vec!["a.txt", "two"]);

    assert!(mount.at("one/two/three/c.txt").exists());
}
