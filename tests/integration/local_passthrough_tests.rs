//! Real-FUSE coverage for the directory under the mount.
use super::common::{TestServer, require_fuse};
use punkfs::{FsNode, PunkFs, StoreApi};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

fn node() -> FsNode {
    FsNode {
        node_id: "local-node".into(),
    }
}

#[test]
fn local_entries_and_creates_operate_on_the_covered_directory() {
    if !require_fuse() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("existing"), b"local-data").unwrap();
    std::os::unix::fs::symlink("existing", root.path().join("link")).unwrap();
    let (session, catalog) = PunkFs::mount_with_local_store(&[], root.path(), node()).unwrap();
    assert_eq!(
        fs::read(root.path().join("existing")).unwrap(),
        b"local-data"
    );
    assert_eq!(
        fs::read_link(root.path().join("link")).unwrap(),
        std::path::PathBuf::from("existing")
    );
    fs::write(root.path().join("existing"), b"short").unwrap();
    fs::set_permissions(
        root.path().join("existing"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(
        fs::metadata(root.path().join("existing"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    fs::write(root.path().join("new"), b"created").unwrap();
    let inode = catalog.get_inode("/new").expect("new file cached");
    let entry = catalog.get_entry(inode).unwrap();
    assert_eq!(entry.node_id, "local-node");
    assert!(!entry.guid.is_empty());
    fs::OpenOptions::new()
        .append(true)
        .open(root.path().join("new"))
        .unwrap()
        .write_all(b"-append")
        .unwrap();
    fs::create_dir(root.path().join("dir")).unwrap();
    fs::rename(root.path().join("new"), root.path().join("dir/moved")).unwrap();
    assert_eq!(
        fs::read(root.path().join("dir/moved")).unwrap(),
        b"created-append"
    );
    drop(session);
    assert_eq!(fs::read(root.path().join("existing")).unwrap(), b"short");
    assert_eq!(
        fs::read(root.path().join("dir/moved")).unwrap(),
        b"created-append"
    );
}

#[test]
fn local_and_remote_entries_merge_and_operations_follow_owner() {
    if !require_fuse() {
        return;
    }
    let server = TestServer::empty();
    server.seed("remote", b"remote-data");
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("local"), b"local-data").unwrap();
    let nodes = vec![("remote-node".to_owned(), server.url.clone())];
    let session = PunkFs::mount_with_local(&nodes, root.path(), node()).unwrap();
    assert_eq!(fs::read(root.path().join("local")).unwrap(), b"local-data");
    assert_eq!(
        fs::read(root.path().join("remote")).unwrap(),
        b"remote-data"
    );
    let error = fs::rename(root.path().join("local"), root.path().join("remote")).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EXDEV));
    assert_eq!(server.backing("remote"), b"remote-data");
    fs::write(root.path().join("new"), b"new-local").unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(root.path().join("remote"))
        .unwrap()
        .write_all(b"-changed")
        .unwrap();
    assert_eq!(server.backing("remote"), b"remote-data-changed");
    assert!(fs::remove_file(root.path().join("remote")).is_err());
    assert_eq!(server.backing("remote"), b"remote-data-changed");
    assert!(!server.backing_exists("new"));
    drop(session);
    assert_eq!(fs::read(root.path().join("new")).unwrap(), b"new-local");
    assert!(!root.path().join("remote").exists());
}

#[test]
fn known_offline_remote_name_is_reserved_for_create() {
    if !require_fuse() {
        return;
    }
    let server = TestServer::empty();
    server.seed("reserved", b"remote");
    let root = tempfile::tempdir().unwrap();
    let session = PunkFs::mount_with_local(
        &[("remote-node".into(), server.url.clone())],
        root.path(),
        node(),
    )
    .unwrap();
    assert!(root.path().join("reserved").exists());
    server.stop();
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.path().join("reserved"));
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EEXIST));
    drop(session);
    assert!(!root.path().join("reserved").exists());
}

#[test]
fn conflicting_local_and_remote_names_cannot_be_read_or_deleted() {
    if !require_fuse() {
        return;
    }
    let server = TestServer::empty();
    server.seed("duplicate", b"remote");
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("duplicate"), b"local").unwrap();
    let session = PunkFs::mount_with_local(
        &[("remote-node".into(), server.url.clone())],
        root.path(),
        node(),
    )
    .unwrap();
    assert!(fs::read(root.path().join("duplicate")).is_err());
    assert!(fs::remove_file(root.path().join("duplicate")).is_err());
    drop(session);
    assert_eq!(fs::read(root.path().join("duplicate")).unwrap(), b"local");
    assert_eq!(server.backing("duplicate"), b"remote");
}

#[test]
fn local_identity_cannot_also_be_a_remote_backend() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        PunkFs::mount_with_local(
            &[("local-node".into(), "http://127.0.0.1:1".into())],
            root.path(),
            node()
        )
        .is_err()
    );
}
