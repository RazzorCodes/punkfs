//! Conformance: writing to a mounted punkfs.
//!
//! These assert the write behaviour ordinary tools depend on. Several are
//! **KNOWN GAP** today because metadata changes (`setattr`) are refused at
//! the FUSE layer; each such test says exactly which syscall and which
//! contract it checks, so the failure is self-explanatory.
//!
//! Nothing in this file creates an entry through the mount: per the current
//! product decision, entries are created by the machine that owns the
//! storage. The tests that check that refusal are in `creation_conformance`.

use std::io::{Read, Seek, SeekFrom, Write};

use crate::common;

#[test]
fn write_through_a_handle_lands_in_the_backing_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("f.txt", b"aaaaaaaaaa");

    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("f.txt"))
            .unwrap();
        file.write_all(b"bbbbbbbbbb").unwrap();
    }

    assert_eq!(
        server.backing("f.txt"),
        b"bbbbbbbbbb",
        "write(2) must reach the backing file"
    );
}

#[test]
fn positional_write_does_not_disturb_other_bytes() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("f.txt", b"0123456789");

    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("f.txt"))
            .unwrap();
        file.seek(SeekFrom::Start(4)).unwrap();
        file.write_all(b"XY").unwrap();
    }

    assert_eq!(server.backing("f.txt"), b"0123XY6789");
}

#[test]
fn append_extends_the_file_and_updates_size() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("log.txt", b"start");

    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(mount.at("log.txt"))
            .unwrap();
        file.write_all(b"-end").unwrap();
    }

    assert_eq!(server.backing("log.txt"), b"start-end");
    assert_eq!(
        std::fs::metadata(mount.at("log.txt")).unwrap().len(),
        9,
        "stat must report the grown size immediately after a write"
    );
}

#[test]
fn writing_past_eof_zero_fills_the_gap() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("sparse.bin", b"ab");

    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("sparse.bin"))
            .unwrap();
        file.seek(SeekFrom::Start(10)).unwrap();
        file.write_all(b"Z").unwrap();
    }

    let backing = server.backing("sparse.bin");
    assert_eq!(backing.len(), 11, "size must span the written offset");
    assert_eq!(&backing[..2], b"ab");
    assert_eq!(
        &backing[2..10],
        &[0u8; 8],
        "the gap between EOF and the write offset must read as zeroes"
    );
    assert_eq!(backing[10], b'Z');
}

/// **Known gap.** `O_TRUNC` is implemented by the kernel as a `setattr` with
/// `size = 0`. We refuse every `setattr`, so the single most common way to
/// create a file's contents (`echo x > file`, `cp`, `>` redirection) fails
/// on an *existing* file.
#[test]
fn truncating_open_of_an_existing_file_succeeds() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("t.txt", b"aaaaaaaaaa");

    let result = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(mount.at("t.txt"))
        .map(|_| ());

    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: open(O_WRONLY|O_TRUNC) on an existing file must succeed\n  got:      {:?}\n  cause:    setattr(size=0) is refused with ENOSYS",
        result.unwrap_err()
    );

    let overwritten = {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("t.txt"))
            .unwrap();
        f.write_all(b"short").unwrap();
        server.backing("t.txt")
    };
    assert_eq!(overwritten, b"short");
}

/// **Known gap.** Same root cause as the truncating open: `ftruncate` is a
/// `setattr`, which is refused.
#[test]
fn ftruncate_shrinks_the_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("t.txt", b"0123456789");

    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(mount.at("t.txt"))
        .unwrap();
    let result = file.set_len(4);

    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: ftruncate(2) to a smaller size must succeed\n  got:      {:?}",
        result.unwrap_err()
    );
    assert_eq!(server.backing("t.txt"), b"0123");
}

/// **Known gap.** `truncate(2)` by path goes through `setattr`.
#[test]
fn truncate_by_path_succeeds() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("t.txt", b"0123456789");

    let result = std::fs::OpenOptions::new()
        .write(true)
        .open(mount.at("t.txt"))
        .and_then(|f| f.set_len(2));

    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: truncate(2) must succeed\n  got:      {:?}",
        result.unwrap_err()
    );
}

#[test]
fn fsync_flushes_without_error() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("f.txt", b"data");

    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(mount.at("f.txt"))
        .unwrap();
    file.sync_all().expect("fsync must succeed on a writable file");
    file.sync_data().expect("fdatasync must succeed");
}

#[test]
fn writes_from_many_handles_do_not_interfere() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("multi.txt", &[b'.'; 40]);

    // Four handles, disjoint ranges. Positional I/O means no shared cursor,
    // so the result must be deterministic.
    for (i, byte) in [b'A', b'B', b'C', b'D'].iter().enumerate() {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("multi.txt"))
            .unwrap();
        file.seek(SeekFrom::Start((i * 10) as u64)).unwrap();
        file.write_all(&[*byte; 10]).unwrap();
    }

    let backing = server.backing("multi.txt");
    assert_eq!(&backing[0..10], &[b'A'; 10]);
    assert_eq!(&backing[10..20], &[b'B'; 10]);
    assert_eq!(&backing[20..30], &[b'C'; 10]);
    assert_eq!(&backing[30..40], &[b'D'; 10]);
}

#[test]
fn a_write_larger_than_one_read_window_round_trips() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("big.bin", &vec![0u8; 1024 * 1024]);

    let payload: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 253) as u8).collect();
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("big.bin"))
            .unwrap();
        file.write_all(&payload).unwrap();
        file.sync_all().unwrap();
    }

    assert_eq!(server.backing("big.bin"), payload);
}

/// **Known gap.** After a write, `stat` must reflect the new size. We do
/// refetch attributes on `getattr`, so this should pass — it is kept as a
/// regression guard for that refetch.
#[test]
fn stat_reflects_a_write_immediately() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("f.txt", b"12345");
    assert_eq!(std::fs::metadata(mount.at("f.txt")).unwrap().len(), 5);

    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("f.txt"))
            .unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"6789").unwrap();
    }

    assert_eq!(
        std::fs::metadata(mount.at("f.txt")).unwrap().len(),
        9,
        "stat must not serve a stale size after a write"
    );
}

#[test]
fn write_to_a_missing_file_is_enoent() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();

    let err = std::fs::OpenOptions::new()
        .write(true)
        .open(mount.at("ghost.txt"))
        .unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "opening a nonexistent file for writing must be ENOENT"
    );
    assert!(!server.backing_exists("ghost.txt"));
}

#[test]
fn read_after_write_sees_the_new_bytes() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("f.txt", b"aaaaaaaa");

    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("f.txt"))
            .unwrap();
        file.write_all(b"ZZZZZZZZ").unwrap();
    }

    let mut got = Vec::new();
    std::fs::File::open(mount.at("f.txt"))
        .unwrap()
        .read_to_end(&mut got)
        .unwrap();
    assert_eq!(got, b"ZZZZZZZZ");
}
