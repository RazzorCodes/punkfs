//! Integration tests for directory read operations.
//!
//! Setup: `tests/common` spawns a real punkfs-server whose root is
//! `tests/testdir` (the fixture). Each test mounts punkfs over a fresh
//! tempdir and exercises it through normal filesystem syscalls.
//!
//! Conventions:
//! - "expected" values are always computed from the fixture on disk,
//!   never hardcoded, so the fixture can grow without editing tests.
//! - Assertions that need features not yet implemented (file content,
//!   `ls -la` metadata) are kept but commented out, marked with TODO.

// Shared test server (tests/common/mod.rs). Path is relative to
// `tests/`, i.e. the sibling directory, because this file is included
// as a module from `tests/integration/main.rs`.
#[path = "../common/mod.rs"]
mod common;

use common::SERVER;

use std::collections::BTreeSet;

use punkfs::PunkFs;

fn testdir() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/testdir").to_string()
}

/// Read the fixture directory through punkfs and compare names with the
/// fixture itself.
#[test]
fn read_directory_lists_children() {
    let testdir = testdir();
    let expected: BTreeSet<String> = std::fs::read_dir(&testdir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!expected.is_empty(), "tests/testdir is empty or missing");

    // Mount point: a fresh tempdir the fs is projected onto.
    let mountpoint = tempfile::tempdir().unwrap();
    let node = punkfs::FsNode {
        node_id: "test-node".into(),
    };
    let _session = PunkFs::mount(&SERVER.url, mountpoint.path(), node).unwrap();

    let names: BTreeSet<String> = std::fs::read_dir(mountpoint.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();

    assert_eq!(names, expected, "directory listing through punkfs differs from the fixture");
}

/// Stat every fixture entry through the mount (stat → getattr → attr_for)
/// and compare kind + size against the fixture itself.
#[test]
fn read_directory_reports_kinds() {
    let testdir = testdir();
    let mountpoint = tempfile::tempdir().unwrap();
    let node = punkfs::FsNode {
        node_id: "test-node".into(),
    };
    let _session = PunkFs::mount(&SERVER.url, mountpoint.path(), node).unwrap();

    for entry in std::fs::read_dir(&testdir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let through_fuse = std::fs::metadata(mountpoint.path().join(&name)).unwrap();
        let on_disk = entry.metadata().unwrap();
        assert_eq!(
            through_fuse.is_dir(),
            on_disk.is_dir(),
            "kind mismatch for {name:?}"
        );
        assert_eq!(
            through_fuse.len(),
            on_disk.len(),
            "size mismatch for {name:?}"
        );
    }
}

// TODO: enable once file content reads are implemented (read RPC).
// Reads every fixture file through the mount and compares bytes.
// #[test]
// fn read_directory_read_contents() {
//     let testdir = testdir();
//     let mountpoint = tempfile::tempdir().unwrap();
//     let node = punkfs::FsNode { node_id: "test-node".into() };
//     let _session = PunkFs::mount(&SERVER.url, mountpoint.path(), node).unwrap();
//
//     for entry in std::fs::read_dir(&testdir).unwrap() {
//         let entry = entry.unwrap();
//         if !entry.file_type().unwrap().is_file() {
//             continue;
//         }
//         let name = entry.file_name();
//         let expected = std::fs::read(entry.path()).unwrap();
//         let got = std::fs::read(mountpoint.path().join(&name)).unwrap();
//         assert_eq!(got, expected, "content mismatch for {name:?}");
//     }
// }

// TODO: enable once setattr/metadata are real (size, mtime, perms).
// Runs `ls -la` on the mount and compares each column against the fixture.
// #[test]
// fn read_directory_ls_la_metadata() {
//     use std::process::Command;
//
//     let testdir = testdir();
//     let mountpoint = tempfile::tempdir().unwrap();
//     let node = punkfs::FsNode { node_id: "test-node".into() };
//     let _session = PunkFs::mount(&SERVER.url, mountpoint.path(), node).unwrap();
//
//     let out = Command::new("ls")
//         .args(["-la", mountpoint.path().to_str().unwrap()])
//         .output()
//         .unwrap();
//     assert!(out.status.success(), "ls -la failed: {}", String::from_utf8_lossy(&out.stderr));
//
//     let got = String::from_utf8_lossy(&out.stdout);
//
//     // Fixture ground truth, restructured the way ls prints it:
//     // "perms owner group size month day time name"
//     let expected = std::fs::read_dir(&testdir)
//         .unwrap()
//         .map(|e| e.unwrap())
//         .collect::<Vec<_>>();
//
//     for entry in &expected {
//         let name = entry.file_name().to_string_lossy().into_owned();
//         assert!(
//             got.contains(&name),
//             "ls -la output missing {name:?}\n--- output ---\n{got}"
//         );
//     }
//     // TODO: compare size/mtime/perms columns per entry once the server
//     // reports real FsMetadata instead of synthesized attrs.
// }
