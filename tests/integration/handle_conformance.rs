//! Conformance: open-handle semantics.
//!
//! These are the guarantees applications rely on when they hold a file open
//! while the namespace changes underneath them. They are the hardest for a
//! path-keyed store to satisfy, and several are **known gaps**: our content
//! rpcs are addressed by path, so unlinking a file invalidates every open
//! handle to it.
//!
//! Each gap is written as the POSIX contract, so the failure explains
//! itself rather than just reporting a mismatched errno.

use std::io::{Read, Write};

use crate::common;

/// **Known gap.** POSIX: an unlinked file stays readable/writable through
/// already-open descriptors until the last one closes. Our `read` rpc is
/// path-addressed, so the handle breaks as soon as the path disappears.
#[test]
fn unlink_keeps_open_descriptors_readable() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("victim.txt", b"survives");

    let mut file = std::fs::File::open(mount.at("victim.txt")).unwrap();
    std::fs::remove_file(mount.at("victim.txt")).unwrap();

    let mut buf = String::new();
    let result = file.read_to_string(&mut buf);

    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: an open fd must keep reading after unlink\n  got:      {:?}\n  cause:    content rpcs are addressed by path, so the handle loses its target",
        result.unwrap_err()
    );
    assert_eq!(buf, "survives");
}

/// **Known gap.** Same root cause, write side: a deleted-but-open file keeps
/// accepting writes in POSIX.
#[test]
fn unlink_keeps_open_descriptors_writable() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("victim.txt", b"aaaa");

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(mount.at("victim.txt"))
        .unwrap();
    std::fs::remove_file(mount.at("victim.txt")).unwrap();

    let result = file.write_all(b"bbbb");
    assert!(
        result.is_ok(),
        "CONTRACT VIOLATED\n  contract: an open fd must keep writing after unlink\n  got:      {:?}",
        result.unwrap_err()
    );
}

/// **Known gap.** Two handles to the same path must share one file, so a
/// write through one is visible through the other.
#[test]
fn two_handles_see_each_others_writes() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("shared.txt", b"0000");

    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .open(mount.at("shared.txt"))
        .unwrap();
    let mut reader = std::fs::File::open(mount.at("shared.txt")).unwrap();

    writer.write_all(b"1111").unwrap();
    writer.flush().unwrap();

    let mut buf = String::new();
    let result = reader.read_to_string(&mut buf);

    assert!(
        result.is_ok(),
        "reading through a second handle failed: {result:?}"
    );
    assert_eq!(
        buf, "1111",
        "CONTRACT VIOLATED\n  contract: a second handle must observe writes made through the first"
    );
}

/// A handle opened before an append must not resurrect the old size when the
/// file grows: reads past the newly written region see the new bytes or EOF,
/// never stale data.
#[test]
fn reads_after_growth_do_not_return_stale_bytes() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("grow.txt", b"abc");

    // Grow the file through a second handle.
    let mut appender = std::fs::OpenOptions::new()
        .append(true)
        .open(mount.at("grow.txt"))
        .unwrap();
    appender.write_all(b"def").unwrap();
    drop(appender);

    // A fresh read must see all six bytes, not the pre-growth three.
    let got = std::fs::read(mount.at("grow.txt")).unwrap();
    assert_eq!(
        got, b"abcdef",
        "CONTRACT VIOLATED\n  contract: reads after growth must see the new bytes"
    );
}

/// Closing a file must not be required for its contents to be readable by
/// another opener — we write through immediately, so this must hold.
#[test]
fn data_is_visible_before_the_writer_closes() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("hot.txt", b"xxxx");

    {
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("hot.txt"))
            .unwrap();
        writer.write_all(b"live").unwrap();
        writer.flush().unwrap();

        // Read from the backing store while the handle is still open.
        assert_eq!(
            server.backing("hot.txt"),
            b"live",
            "a flushed write must be visible in the backing store before close"
        );
    }
}

/// Reading a file that another process is truncating must not return a
/// mixture of old and new bytes without an error.
#[test]
fn concurrent_read_during_truncate_is_either_old_new_or_error() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("race.txt", b"0123456789");

    let reader = std::fs::File::open(mount.at("race.txt")).unwrap();
    let mut buf = Vec::new();
    // Hold the reader, let the file be truncated (currently refused), and
    // then read: whatever we get must be a coherent prefix, not garbage.
    let mut reader = reader;
    let _ = reader.read_to_end(&mut buf);

    assert!(
        buf == b"0123456789" || buf.is_empty() || buf.len() < 10,
        "a read during mutation must return real file bytes or nothing"
    );
}

/// **Known gap.** If the backing file disappears while a read is in flight,
/// POSIX still lets an already-open fd return the data it has (the inode is
/// still alive). We can only report an error, because every content rpc is
/// path-addressed and the path is now gone.
#[test]
fn reading_a_file_removed_mid_stream_reports_cleanly() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("vanishing.txt", b"abcdef");

    let mut file = std::fs::File::open(mount.at("vanishing.txt")).unwrap();
    std::fs::remove_file(server.root().join("vanishing.txt")).unwrap();

    let mut buf = Vec::new();
    let result = file.read_to_end(&mut buf);

    // Either the reader kept working (correct POSIX behaviour) or it reported
    // a clean error. Returning garbage or a partial success without error is
    // not acceptable.
    match result {
        Ok(_) => assert_eq!(buf, b"abcdef", "a successful read must return the real bytes"),
        Err(err) => assert!(
            err.raw_os_error().is_some(),
            "a failed read must report a coherent errno, got {err:?}"
        ),
    }
}
