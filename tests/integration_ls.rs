mod common;
use common::SERVER;

use punkfs::PunkFs;
use std::collections::BTreeSet;

#[test]
fn ls_root_lists_children() {
    // Ground truth: whatever is really in tests/testdir.
    let testdir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/testdir");
    let expected: BTreeSet<String> = std::fs::read_dir(testdir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!expected.is_empty(), "tests/testdir is empty or missing");

    let dir = tempfile::tempdir().unwrap();
    let _session = PunkFs::mount(&SERVER.url, dir.path()).unwrap();

    let names: BTreeSet<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();

    assert_eq!(names, expected);
}
