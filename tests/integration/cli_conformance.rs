//! CLI-level conformance: real executables against the mount.
//!
//! This is the layer that proves ordinary programs work, not just our own
//! syscall calls. Each test asserts the command's outcome *and* the effect on
//! the backing directory.
//!
//! Tests marked **Known gap** fail today for a documented reason; the panic
//! message names the tool, the contract, and the observed error.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

use crate::common;

fn run(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {program}: {e}"))
}

/// Assert a command succeeded, quoting its stderr if it did not.
fn expect_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed (exit {:?})\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Assert a command failed, so a silent success is caught.
fn expect_fail(out: &Output, what: &str) {
    assert!(
        !out.status.success(),
        "{what} unexpectedly succeeded\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// --- listing and metadata ------------------------------------------------

#[test]
fn ls_lists_every_entry() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("one.txt", b"1");
    server.seed("two.txt", b"2");
    server.seed_dir("three");

    let out = run("ls", &[mount.path().to_str().unwrap()]);
    expect_ok(&out, "ls");

    let listing = String::from_utf8_lossy(&out.stdout);
    for name in ["one.txt", "two.txt", "three"] {
        assert!(listing.contains(name), "ls did not list {name}: {listing}");
    }
}

#[test]
fn ls_la_reports_the_real_size_and_kind() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("sized.txt", &vec![b'x'; 1234]);
    server.seed_dir("adir");

    let out = run("ls", &["-la", mount.path().to_str().unwrap()]);
    expect_ok(&out, "ls -la");
    let listing = String::from_utf8_lossy(&out.stdout);

    let file_line = listing
        .lines()
        .find(|l| l.ends_with("sized.txt"))
        .unwrap_or_else(|| panic!("sized.txt missing from ls -la:\n{listing}"));
    assert!(
        file_line.contains("1234"),
        "ls -la must show the real size 1234: {file_line}"
    );

    let dir_line = listing
        .lines()
        .find(|l| l.ends_with("adir"))
        .unwrap_or_else(|| panic!("adir missing from ls -la:\n{listing}"));
    assert!(
        dir_line.starts_with('d'),
        "a directory must be marked with 'd': {dir_line}"
    );
}

#[test]
fn ls_l_shows_a_raw_mode_matching_the_backing_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("m.txt", b"x");
    std::fs::set_permissions(
        server.root().join("m.txt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();

    let out = run("ls", &["-l", mount.at("m.txt").to_str().unwrap()]);
    expect_ok(&out, "ls -l");
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(
        line.starts_with("-rw-r-----"),
        "the permission column must be -rw-r-----, got: {line}"
    );
}

#[test]
fn stat_reports_the_backing_size() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("s.txt", b"12345");

    let out = run("stat", &["-c", "%s", mount.at("s.txt").to_str().unwrap()]);
    expect_ok(&out, "stat");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "5");
}

#[test]
fn du_counts_real_bytes() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("blob.bin", &vec![b'x'; 40 * 1024]);

    let out = run("du", &["-sb", mount.at("blob.bin").to_str().unwrap()]);
    expect_ok(&out, "du");
    let reported: u64 = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        reported,
        40 * 1024,
        "du -b must report the file's real length"
    );
}

// --- content -------------------------------------------------------------

#[test]
fn cat_prints_exact_contents() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("c.txt", b"file contents\n");

    let out = run("cat", &[mount.at("c.txt").to_str().unwrap()]);
    expect_ok(&out, "cat");
    assert_eq!(out.stdout, b"file contents\n");
}

#[test]
fn cmp_agrees_between_mount_and_backing() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    let payload: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
    server.seed("payload.bin", &payload);

    let out = run(
        "cmp",
        &[
            server.root().join("payload.bin").to_str().unwrap(),
            mount.at("payload.bin").to_str().unwrap(),
        ],
    );
    expect_ok(&out, "cmp");
}

#[test]
fn wc_counts_the_real_byte_count() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("w.txt", &vec![b'a'; 4321]);

    let out = run("wc", &["-c", mount.at("w.txt").to_str().unwrap()]);
    expect_ok(&out, "wc");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).split_whitespace().next(),
        Some("4321")
    );
}

#[test]
fn grep_finds_a_pattern_inside_a_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("haystack.txt", b"alpha\nneedle\nomega\n");

    let out = run("grep", &["needle", mount.at("haystack.txt").to_str().unwrap()]);
    expect_ok(&out, "grep");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "needle\n");
}

#[test]
fn head_and_tail_read_short_ranges() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("lines.txt", b"1\n2\n3\n4\n5\n6\n7\n8\n9\n");

    let head = run("head", &["-n", "2", mount.at("lines.txt").to_str().unwrap()]);
    expect_ok(&head, "head");
    assert_eq!(String::from_utf8_lossy(&head.stdout), "1\n2\n");

    let tail = run("tail", &["-n", "2", mount.at("lines.txt").to_str().unwrap()]);
    expect_ok(&tail, "tail");
    assert_eq!(String::from_utf8_lossy(&tail.stdout), "8\n9\n");
}

#[test]
fn dd_reads_and_writes_block_sized_chunks() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    let size = 4 * 4096;
    server.seed("blocks.bin", &vec![0u8; size]);

    let script = format!(
        "dd if=/dev/urandom of='{}' bs=4096 count=4 conv=notrunc status=none",
        mount.at("blocks.bin").display()
    );
    let out = run("sh", &["-c", &script]);
    expect_ok(&out, "dd write");

    let backing = server.backing("blocks.bin");
    assert_eq!(backing.len(), size);
    assert!(
        backing.iter().any(|b| *b != 0),
        "dd must have written real data"
    );
    assert_eq!(
        std::fs::read(mount.at("blocks.bin")).unwrap(),
        backing,
        "the mount and the server must agree on the bytes"
    );
}

#[test]
fn cp_from_the_mount_preserves_contents() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    let payload: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
    server.seed("src.bin", &payload);

    let dest = tempfile::tempdir().unwrap();
    let out = run(
        "cp",
        &[
            mount.at("src.bin").to_str().unwrap(),
            dest.path().join("copy.bin").to_str().unwrap(),
        ],
    );
    expect_ok(&out, "cp from mount");
    assert_eq!(std::fs::read(dest.path().join("copy.bin")).unwrap(), payload);
}

#[test]
fn tar_round_trips_the_tree() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("tree/a.txt", b"aaa");
    server.seed("tree/nested/b.txt", b"bbb");

    let archive = tempfile::tempdir().unwrap();
    let tarball = archive.path().join("out.tar");
    let out = run(
        "tar",
        &[
            "-cf",
            tarball.to_str().unwrap(),
            "-C",
            mount.at("tree").to_str().unwrap(),
            ".",
        ],
    );
    expect_ok(&out, "tar create");

    let extract = tempfile::tempdir().unwrap();
    let out = run(
        "tar",
        &["-xf", tarball.to_str().unwrap(), "-C", extract.path().to_str().unwrap()],
    );
    expect_ok(&out, "tar extract");

    assert_eq!(std::fs::read(extract.path().join("a.txt")).unwrap(), b"aaa");
    assert_eq!(
        std::fs::read(extract.path().join("nested/b.txt")).unwrap(),
        b"bbb"
    );
}

// --- mutation ------------------------------------------------------------

#[test]
fn mv_renames_a_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("old.txt", b"body");

    let out = run(
        "mv",
        &[
            mount.at("old.txt").to_str().unwrap(),
            mount.at("new.txt").to_str().unwrap(),
        ],
    );
    expect_ok(&out, "mv");

    assert!(!server.backing_exists("old.txt"));
    assert_eq!(server.backing("new.txt"), b"body");
}

#[test]
fn mv_renames_a_directory() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("d/inner.txt", b"x");

    let out = run(
        "mv",
        &[
            mount.at("d").to_str().unwrap(),
            mount.at("d2").to_str().unwrap(),
        ],
    );
    expect_ok(&out, "mv dir");

    assert!(!server.backing_exists("d/inner.txt"));
    assert_eq!(server.backing("d2/inner.txt"), b"x");
}

#[test]
fn rm_deletes_a_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("bye.txt", b"x");

    expect_ok(&run("rm", &[mount.at("bye.txt").to_str().unwrap()]), "rm");
    assert!(!server.backing_exists("bye.txt"));
}

#[test]
fn rm_r_deletes_a_populated_tree() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("junk/a.txt", b"a");
    server.seed("junk/sub/b.txt", b"b");

    expect_ok(
        &run("rm", &["-r", mount.at("junk").to_str().unwrap()]),
        "rm -r",
    );
    assert!(!server.backing_exists("junk"));
}

#[test]
fn rmdir_deletes_an_empty_directory() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed_dir("gone");

    expect_ok(
        &run("rmdir", &[mount.at("gone").to_str().unwrap()]),
        "rmdir",
    );
    assert!(!server.backing_exists("gone"));
}

#[test]
fn find_walks_the_whole_tree() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("x/y/leaf.txt", b"l");
    server.seed("x/other.txt", b"o");

    let out = run("find", &[mount.path().to_str().unwrap(), "-type", "f"]);
    expect_ok(&out, "find");

    let found = String::from_utf8_lossy(&out.stdout);
    assert!(found.contains("leaf.txt"), "find missed leaf.txt: {found}");
    assert!(found.contains("other.txt"), "find missed other.txt: {found}");
    assert_eq!(
        found.lines().count(),
        2,
        "find must report exactly the two files: {found}"
    );
}

#[test]
fn df_reports_the_mount_with_real_capacity() {
    if !common::require_fuse() {
        return;
    }
    let (_server, mount) = common::mount_empty();

    let out = run("df", &["-P", mount.path().to_str().unwrap()]);
    expect_ok(&out, "df");
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(
        report.contains("punkfs"),
        "df must name the filesystem: {report}"
    );
    // The capacity column must not be a zero, which would make df useless.
    let line = report.lines().nth(1).unwrap_or("");
    let blocks: u64 = line.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    assert!(blocks > 0, "df must report real capacity, got: {line}");
}

// --- refusals that must be visible ---------------------------------------

#[test]
fn touch_on_a_missing_file_fails_visibly() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();

    expect_fail(
        &run("touch", &[mount.at("new.txt").to_str().unwrap()]),
        "touch",
    );
    assert!(!server.backing_exists("new.txt"));
}

#[test]
fn mkdir_fails_visibly() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();

    expect_fail(
        &run("mkdir", &[mount.at("newdir").to_str().unwrap()]),
        "mkdir",
    );
    assert!(!server.backing_exists("newdir"));
}

/// **Known gap.** `chmod` needs `setattr`, which is refused, so the mode never
/// changes even though `ls -l` shows a real mode.
#[test]
fn chmod_updates_the_backing_mode() {
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

    let result = run("chmod", &["600", mount.at("c.txt").to_str().unwrap()]);

    assert!(
        result.status.success(),
        "CONTRACT VIOLATED\n  command:  chmod 600 {}\n  contract: chmod must succeed and change the backing mode\n  stderr:   {}\n  cause:    setattr is refused with ENOSYS",
        mount.at("c.txt").display(),
        String::from_utf8_lossy(&result.stderr),
    );
}

/// **Known gap.** `cp` onto an existing file truncates, which needs
/// `setattr`.
#[test]
fn cp_over_an_existing_file_succeeds() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("src.txt", b"replacement");
    server.seed("dst.txt", b"old contents that are longer");

    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("incoming.txt"), b"replacement").unwrap();

    let result = run(
        "cp",
        &[
            src.path().join("incoming.txt").to_str().unwrap(),
            mount.at("dst.txt").to_str().unwrap(),
        ],
    );

    assert!(
        result.status.success(),
        "CONTRACT VIOLATED\n  command:  cp <file> {}\n  contract: overwriting an existing file must succeed\n  stderr:   {}\n  cause:    cp opens with O_TRUNC, which needs setattr",
        mount.at("dst.txt").display(),
        String::from_utf8_lossy(&result.stderr),
    );
    assert_eq!(server.backing("dst.txt"), b"replacement");
}

/// **Known gap.** `echo > file` is `O_TRUNC` on an existing file.
#[test]
fn shell_redirection_truncates_an_existing_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("log.txt", b"old long contents here");

    let script = format!("echo 'short' > '{}'", mount.at("log.txt").display());
    let result = run("sh", &["-c", &script]);

    assert!(
        result.status.success(),
        "CONTRACT VIOLATED\n  command:  echo 'short' > {}\n  contract: shell redirection must truncate and write\n  stderr:   {}\n  cause:    O_TRUNC needs setattr",
        mount.at("log.txt").display(),
        String::from_utf8_lossy(&result.stderr),
    );
    assert_eq!(server.backing("log.txt"), b"short\n");
}

#[test]
fn appending_through_redirection_works() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("log.txt", b"line1\n");

    let script = format!("printf 'line2\\n' >> '{}'", mount.at("log.txt").display());
    expect_ok(&run("sh", &["-c", &script]), "append");
    assert_eq!(server.backing("log.txt"), b"line1\nline2\n");
}

/// **Known gap.** `truncate -s` is `setattr`.
#[test]
fn truncate_command_shrinks_the_file() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("t.txt", b"0123456789");

    let result = run(
        "truncate",
        &["-s", "4", mount.at("t.txt").to_str().unwrap()],
    );

    assert!(
        result.status.success(),
        "CONTRACT VIOLATED\n  command:  truncate -s 4 {}\n  contract: truncate must shrink the file\n  stderr:   {}\n  cause:    setattr is refused with ENOSYS",
        mount.at("t.txt").display(),
        String::from_utf8_lossy(&result.stderr),
    );
    assert_eq!(server.backing("t.txt"), b"0123");
}
