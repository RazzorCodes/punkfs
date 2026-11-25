//! Symlinks must behave as namespace entries, regardless of their owner.
use crate::common::{self, TestServer};
use punkfs::FsNode;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

#[test]
fn local_symlinks_follow_normal_namespace_semantics() {
    if !common::require_fuse() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("file"), b"body").unwrap();
    fs::create_dir(root.path().join("dir")).unwrap();
    fs::write(root.path().join("dir/nested"), b"nested").unwrap();
    symlink("file", root.path().join("existing")).unwrap();
    let (session, cache) = crate::common::mount_local(
        &[],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
        false,
    )
    .unwrap();
    let at = |name: &str| root.path().join(name);
    assert!(
        fs::symlink_metadata(at("existing"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(at("existing")).unwrap(), Path::new("file"));
    assert_eq!(fs::read(at("existing")).unwrap(), b"body");
    assert_eq!(
        fs::read_link(at("file")).unwrap_err().raw_os_error(),
        Some(libc::EINVAL)
    );
    symlink("file", at("created")).unwrap();
    assert_eq!(fs::read(at("created")).unwrap(), b"body");
    use std::os::unix::fs::OpenOptionsExt;
    let err = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(at("created"))
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ELOOP));
    let err = symlink("file", at("created")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
    assert_eq!(fs::read_link(at("created")).unwrap(), Path::new("file"));
    symlink("created", at("chain")).unwrap();
    assert_eq!(fs::read(at("chain")).unwrap(), b"body");
    symlink("missing", at("dangling")).unwrap();
    assert!(
        fs::symlink_metadata(at("dangling"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(at("dangling")).unwrap(), Path::new("missing"));
    assert_eq!(
        fs::read(at("dangling")).unwrap_err().raw_os_error(),
        Some(libc::ENOENT)
    );
    symlink("dir", at("dirlink")).unwrap();
    assert_eq!(fs::read(at("dirlink/nested")).unwrap(), b"nested");
    symlink("loop-b", at("loop-a")).unwrap();
    symlink("loop-a", at("loop-b")).unwrap();
    assert_eq!(
        fs::read(at("loop-a")).unwrap_err().raw_os_error(),
        Some(libc::ELOOP)
    );
    use punkfs::StoreApi;
    let before = cache.get_inode("/created").unwrap();
    fs::rename(at("created"), at("renamed")).unwrap();
    assert_eq!(fs::read_link(at("renamed")).unwrap(), Path::new("file"));
    assert_eq!(
        cache.get_inode("/renamed"),
        Some(before),
        "rename must preserve local inode identity"
    );
    fs::remove_file(at("renamed")).unwrap();
    assert_eq!(fs::read(at("file")).unwrap(), b"body");

    let raw = std::ffi::OsString::from_vec(vec![b'x', 0xff]);
    symlink(&raw, at("bytes")).unwrap();
    assert_eq!(
        fs::read_link(at("bytes")).unwrap().as_os_str().as_bytes(),
        raw.as_bytes()
    );
    drop(session);
}

#[test]
fn symlink_targets_resolve_in_the_merged_namespace() {
    if !common::require_fuse() {
        return;
    }
    let server = TestServer::empty();
    server.seed("remote-file", b"remote");
    let raw_remote = std::ffi::OsString::from_vec(vec![b'r', 0xfe]);
    symlink(&raw_remote, server.root().join("raw-link")).unwrap();
    symlink("local-file", server.root().join("remote-link")).unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("local-file"), b"local").unwrap();
    symlink("remote-file", root.path().join("local-link")).unwrap();
    symlink("remote-file", root.path().join("collision")).unwrap();
    server.seed("collision", b"remote collision");
    let (session, cache) = crate::common::mount_local(
        &[("peer".into(), server.url.clone())],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
        false,
    )
    .unwrap();
    assert!(
        fs::read_link(root.path().join("collision")).is_err(),
        "conflicting link and file must not be guessed"
    );
    assert_eq!(fs::read(root.path().join("local-link")).unwrap(), b"remote");
    assert_eq!(
        fs::read_link(root.path().join("remote-file"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EINVAL)
    );
    assert_eq!(fs::read(root.path().join("remote-link")).unwrap(), b"local");
    assert_eq!(
        fs::read_link(root.path().join("raw-link"))
            .unwrap()
            .as_os_str()
            .as_bytes(),
        raw_remote.as_bytes()
    );
    assert_eq!(
        fs::read_link(root.path().join("remote-link")).unwrap(),
        Path::new("local-file")
    );
    use punkfs::StoreApi;
    let before = cache.get_inode("/remote-link").unwrap();
    fs::rename(
        root.path().join("remote-link"),
        root.path().join("renamed-remote"),
    )
    .unwrap();
    assert_eq!(
        fs::read_link(root.path().join("renamed-remote")).unwrap(),
        Path::new("local-file")
    );
    assert_eq!(
        cache.get_inode("/renamed-remote"),
        Some(before),
        "remote rename must preserve local inode identity"
    );
    fs::remove_file(root.path().join("renamed-remote")).unwrap();
    assert_eq!(fs::read(root.path().join("local-file")).unwrap(), b"local");
    assert!(!server.root().join("renamed-remote").exists());
    symlink("remote-file", root.path().join("created-link")).unwrap();
    assert_eq!(
        fs::read(root.path().join("created-link")).unwrap(),
        b"remote"
    );
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("outside"), b"outside").unwrap();
    let absolute: PathBuf = external.path().join("outside");
    symlink(&absolute, root.path().join("absolute")).unwrap();
    assert_eq!(
        fs::read_link(root.path().join("absolute")).unwrap(),
        absolute
    );
    assert_eq!(fs::read(root.path().join("absolute")).unwrap(), b"outside");
    drop(session);
}
