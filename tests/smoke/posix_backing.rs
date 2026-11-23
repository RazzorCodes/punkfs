//! Platform reference smoke tests, not assertions about punkfs itself.
//! They keep expected Unix syscall behavior executable alongside product tests.
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[test]
fn reference_mode_zero_remains_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f");
    fs::write(&path, b"x").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0);
}

#[test]
fn reference_unlinked_open_handle_remains_readable() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f");
    fs::write(&path, b"x").unwrap();
    let mut file = fs::File::open(&path).unwrap();
    fs::remove_file(path).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"x");
}

#[test]
fn reference_hard_link_shares_inode() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    fs::write(&a, b"x").unwrap();
    fs::hard_link(&a, &b).unwrap();
    assert_eq!(
        fs::metadata(a).unwrap().ino(),
        fs::metadata(b).unwrap().ino()
    );
}
