//! Low-level contracts through the public store and backing-service APIs.
//! These exercise behaviors accessible below FUSE; absent store operations
//! are recorded as explicit uncovered gaps rather than fake passing tests.
use crate::common::TestServer;
use crate::store_test_api::TestStoreApi;
use punkfs::{FsNode, SetattrChanges, Store, StoreApi};
use std::os::unix::fs::PermissionsExt;

// POSIX reference oracle: the lower-level backing behavior must be capable of
// supporting each syscall contract. This is explicitly *not* a punkfs/FUSE
// assertion; it keeps reference expectations independently executable while
// operation-specific Store tests below exercise the APIs that actually exist.
fn backing_reference(contract: fn(&std::path::Path), mode: u32) {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    server.seed("other", b"other");
    std::fs::set_permissions(
        server.root().join("data"),
        std::fs::Permissions::from_mode(mode),
    )
    .unwrap();
    contract(server.root());
}
macro_rules! reference_contracts {
    ($($name:ident => $contract:ident $(, mode = $mode:expr)?;)+) => {$ (
        #[test]
        fn $name() { reference_contracts!(@run $contract $(, $mode)?); }
    )+};
    (@run $contract:ident, $mode:expr) => { backing_reference(crate::shared_posix_contract::$contract, $mode) };
    (@run $contract:ident) => { backing_reference(crate::shared_posix_contract::$contract, 0o600) };
}
reference_contracts! {
    reference_remote_truncate => truncate;
    reference_remote_chmod => mode;
    reference_nanosecond_timestamps => timestamps;
    reference_open_handle_after_unlink => open_handle_unlink;
    reference_open_handle_after_rename => open_handle_rename;
    reference_open_handle_after_replacement => open_handle_replaced_destination;
    reference_zero_mode => permissions, mode = 0;
    reference_access_denies_mode_zero => access_checks, mode = 0;
    reference_access_denies_without_execute_bits => access_executable;
    reference_ownership => ownership;
    reference_hard_links => hard_link;
    reference_local_hard_links => local_hard_link;
    reference_fifo => fifo;
    reference_extended_attributes => xattr;
    reference_list_extended_attributes => xattr_list;
    reference_acl => acl_roundtrip;
    reference_non_utf8_filename => byte_filename;
    reference_create_file_and_directory => create_file_and_directory;
    reference_symlink => symlink_semantics;
    reference_concurrent_append => concurrent_append;
    reference_exclusive_create => concurrent_create_exclusive;
    reference_advisory_lock => advisory_lock;
    reference_sync => fsync_data;
    reference_mmap => mmap_read;
    reference_rename_nonempty => rename_nonempty;
    reference_rename_noreplace => rename_noreplace;
    reference_rename_exchange => rename_exchange;
}

fn store(server: &TestServer) -> Store {
    Store::new(
        &server.url,
        FsNode {
            node_id: "reader".into(),
        },
    )
    .unwrap()
}

fn seeded_xattr_reference(contract: fn(&std::path::Path)) {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    if let Err(err) = crate::shared_posix_contract::seed_xattr(&server.root().join("data")) {
        if matches!(
            err.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::EPERM)
        ) {
            eprintln!("skipping xattr: backing does not support it: {err}");
            return;
        }
        panic!("seed xattr: {err}");
    }
    contract(server.root());
}

#[test]
fn reference_seeded_xattr_can_be_read_listed_removed() {
    seeded_xattr_reference(crate::shared_posix_contract::seeded_xattr);
}
#[test]
fn reference_seeded_xattr_can_be_listed() {
    seeded_xattr_reference(crate::shared_posix_contract::seeded_xattr_list);
}
#[test]
fn reference_seeded_xattr_can_be_removed() {
    seeded_xattr_reference(crate::shared_posix_contract::seeded_xattr_remove);
}

#[test]
fn functional_xattrs_round_trip_and_protect_punkfs_guid() {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    if let Err(err) = crate::shared_posix_contract::seed_xattr(&server.root().join("data")) {
        if matches!(
            err.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::EPERM)
        ) {
            eprintln!("skipping xattr: backing does not support it: {err}");
            return;
        }
        panic!("seed xattr: {err}");
    }
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    assert_eq!(
        store.set_xattr("/data", b"user.example", b"hello", libc::XATTR_REPLACE),
        Err(punkfs::StoreError::Local(libc::ENODATA))
    );
    store
        .set_xattr("/data", b"user.example", b"hello", libc::XATTR_CREATE)
        .unwrap();
    assert_eq!(
        store.set_xattr("/data", b"user.example", b"again", libc::XATTR_CREATE),
        Err(punkfs::StoreError::Local(libc::EEXIST))
    );
    assert_eq!(store.get_xattr("/data", b"user.example").unwrap(), b"hello");
    let names = store.list_xattr("/data").unwrap();
    assert!(names.split(|b| *b == 0).any(|n| n == b"user.example"));
    assert!(!names.split(|b| *b == 0).any(|n| n == b"user.punkfs.guid"));
    assert_eq!(
        store.set_xattr("/data", b"user.punkfs.guid", b"other", 0),
        Err(punkfs::StoreError::Local(libc::EPERM))
    );
    assert_eq!(
        store.remove_xattr("/data", b"user.punkfs.guid"),
        Err(punkfs::StoreError::Local(libc::EPERM))
    );
    store.remove_xattr("/data", b"user.example").unwrap();
    assert_eq!(
        store.get_xattr("/data", b"user.example"),
        Err(punkfs::StoreError::Local(libc::ENODATA))
    );
}

#[test]
fn functional_remote_directory_xattrs_round_trip() {
    let server = TestServer::empty();
    let directory = server.root().join("directory");
    std::fs::create_dir(&directory).unwrap();
    if let Err(err) = crate::shared_posix_contract::seed_xattr(&directory) {
        if matches!(
            err.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::EPERM)
        ) {
            eprintln!("skipping directory xattr: backing does not support it: {err}");
            return;
        }
        panic!("probe directory xattr: {err}");
    }
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    store
        .set_xattr("/directory", b"user.example", b"value", libc::XATTR_CREATE)
        .unwrap();
    assert_eq!(
        store.get_xattr("/directory", b"user.example").unwrap(),
        b"value"
    );
    let names = store.list_xattr("/directory").unwrap();
    assert!(names.split(|b| *b == 0).any(|name| name == b"user.example"));
    assert!(
        !names
            .split(|b| *b == 0)
            .any(|name| name == b"user.punkfs.guid")
    );
    store.remove_xattr("/directory", b"user.example").unwrap();
    assert_eq!(
        store.get_xattr("/directory", b"user.example"),
        Err(punkfs::StoreError::Local(libc::ENODATA))
    );
}

#[test]
fn functional_setattr_truncates_the_owner_file() {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    store
        .setattr(
            "/data",
            SetattrChanges {
                size: Some(2),
                ..Default::default()
            },
        )
        .expect("backend truncation");
    assert_eq!(server.backing("data"), b"ab");
}

#[test]
fn functional_setattr_retains_backing_ownership() {
    use std::os::unix::fs::MetadataExt;
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    let metadata = std::fs::metadata(server.root().join("data")).unwrap();
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    let attr = store
        .setattr(
            "/data",
            SetattrChanges {
                uid: Some(metadata.uid()),
                gid: Some(metadata.gid()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        (attr.access.uid, attr.access.gid),
        (metadata.uid(), metadata.gid())
    );
    let after = std::fs::metadata(server.root().join("data")).unwrap();
    assert_eq!((after.uid(), after.gid()), (metadata.uid(), metadata.gid()));
}

#[test]
fn functional_setattr_updates_mode() {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    store
        .setattr(
            "/data",
            SetattrChanges {
                mode: Some(0o600),
                ..Default::default()
            },
        )
        .expect("backend chmod");
    assert_eq!(
        std::fs::metadata(server.root().join("data"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn functional_setattr_updates_mtime_seconds() {
    use std::os::unix::fs::MetadataExt;
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    store
        .setattr(
            "/data",
            SetattrChanges {
                modified_unix: Some(1_000_001),
                modified_nanos: Some(123_456_789),
                ..Default::default()
            },
        )
        .expect("backend utimens");
    let metadata = std::fs::metadata(server.root().join("data")).unwrap();
    assert_eq!(
        (metadata.mtime(), metadata.mtime_nsec()),
        (1_000_001, 123_456_789)
    );
    let attr = store.refresh("/data").unwrap();
    assert_eq!(
        attr.times
            .modified
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos(),
        123_456_789
    );
}

#[test]
fn functional_epoch_timestamp_is_preserved_by_store() {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    store
        .setattr(
            "/data",
            SetattrChanges {
                modified_unix: Some(0),
                modified_nanos: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        store.refresh("/data").unwrap().times.modified,
        std::time::UNIX_EPOCH
    );
}

#[test]
fn functional_pre_epoch_timestamp_and_omit() {
    use std::os::unix::fs::MetadataExt;
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    store
        .setattr(
            "/data",
            SetattrChanges {
                accessed_unix: Some(100),
                accessed_nanos: Some(123_456_789),
                ..Default::default()
            },
        )
        .unwrap();
    store
        .setattr(
            "/data",
            SetattrChanges {
                modified_unix: Some(-2),
                modified_nanos: Some(750_000_000),
                ..Default::default()
            },
        )
        .unwrap();
    let meta = std::fs::metadata(server.root().join("data")).unwrap();
    assert_eq!((meta.atime(), meta.atime_nsec()), (100, 123_456_789));
    assert_eq!((meta.mtime(), meta.mtime_nsec()), (-2, 750_000_000));
    assert_eq!(
        store.refresh("/data").unwrap().times.modified,
        std::time::UNIX_EPOCH - std::time::Duration::from_millis(1250)
    );
}

#[test]
fn functional_same_owner_rename_noreplace_preserves_destination() {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    server.seed("other", b"other");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    assert!(
        store
            .rename("/data", "/other", fuser::RenameFlags::RENAME_NOREPLACE)
            .is_err()
    );
    assert_eq!(server.backing("data"), b"abcdef");
    assert_eq!(server.backing("other"), b"other");
}

#[test]
fn functional_same_owner_rename_exchange_swaps_files() {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    server.seed("other", b"other");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    let first = store.get_inode("/data").unwrap();
    let second = store.get_inode("/other").unwrap();
    store
        .rename("/data", "/other", fuser::RenameFlags::RENAME_EXCHANGE)
        .expect("same-owner rename exchange");
    assert_eq!(store.get_inode("/data"), Some(second));
    assert_eq!(store.get_inode("/other"), Some(first));
    assert_eq!(server.backing("data"), b"other");
    assert_eq!(server.backing("other"), b"abcdef");
}

#[test]
fn functional_cross_owner_exchange_is_refused_without_data_loss() {
    let left = TestServer::empty();
    let right = TestServer::empty();
    left.seed("left", b"left");
    right.seed("right", b"right");
    let store = Store::new_nodes(
        &[
            ("left-node".into(), left.url.clone()),
            ("right-node".into(), right.url.clone()),
        ],
        FsNode {
            node_id: "reader".into(),
        },
    )
    .unwrap();
    store.ensure_listed("/").unwrap();
    assert_eq!(
        store.rename("/left", "/right", fuser::RenameFlags::RENAME_EXCHANGE),
        Err(punkfs::StoreError::Local(libc::EXDEV))
    );
    assert_eq!(left.backing("left"), b"left");
    assert_eq!(right.backing("right"), b"right");
}

#[test]
fn functional_same_owner_exchange_swaps_cached_directories_and_descendants() {
    let server = TestServer::empty();
    server.seed("left/nested/file", b"left");
    server.seed("right/nested/file", b"right");
    let store = store(&server);
    for dir in ["/", "/left", "/right", "/left/nested", "/right/nested"] {
        store.ensure_listed(dir).unwrap();
    }
    let left = store.get_inode("/left").unwrap();
    let right = store.get_inode("/right").unwrap();
    let left_file = store.get_inode("/left/nested/file").unwrap();
    let right_file = store.get_inode("/right/nested/file").unwrap();
    store
        .rename("/left", "/right", fuser::RenameFlags::RENAME_EXCHANGE)
        .unwrap();
    assert_eq!(store.get_inode("/left"), Some(right));
    assert_eq!(store.get_inode("/right"), Some(left));
    assert_eq!(store.get_inode("/left/nested/file"), Some(right_file));
    assert_eq!(store.get_inode("/right/nested/file"), Some(left_file));
    assert_eq!(server.backing("left/nested/file"), b"right");
    assert_eq!(server.backing("right/nested/file"), b"left");
}

#[test]
fn functional_mode_zero_is_preserved_by_store() {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    std::fs::set_permissions(
        server.root().join("data"),
        std::fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    let ino = store.get_inode("/data").unwrap();
    assert_eq!(store.get_entry(ino).unwrap().attr.access.mode, 0);
}

#[test]
fn functional_zero_gid_is_preserved_by_store() {
    use std::os::unix::fs::MetadataExt;
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    let path =
        std::ffi::CString::new(server.root().join("data").as_os_str().as_encoded_bytes()).unwrap();
    if unsafe { libc::chown(path.as_ptr(), u32::MAX, 0) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EPERM) {
            eprintln!("skipping gid-zero ownership test: no permission to set backing gid 0");
            return;
        }
        panic!("set backing gid: {err}");
    }
    let expected = std::fs::metadata(server.root().join("data")).unwrap();
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    let ino = store.get_inode("/data").unwrap();
    let access = store.get_entry(ino).unwrap().attr.access;
    assert_eq!((access.uid, access.gid), (expected.uid(), 0));
}

#[test]
fn functional_symlink_target_bytes_are_returned_exactly() {
    use std::os::unix::ffi::OsStringExt;
    let server = TestServer::empty();
    let target = std::ffi::OsString::from_vec(vec![b'x', 0xff]);
    std::os::unix::fs::symlink(&target, server.root().join("link")).unwrap();
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    assert_eq!(store.readlink("/link").unwrap(), vec![b'x', 0xff]);
}

#[test]
fn functional_rename_nonempty_directory_preserves_both() {
    let server = TestServer::empty();
    server.seed_dir("source");
    server.seed("dest/keep", b"keep");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    assert!(
        store
            .rename("/source", "/dest", fuser::RenameFlags::empty())
            .is_err()
    );
    assert!(server.root().join("source").is_dir());
    assert_eq!(server.backing("dest/keep"), b"keep");
}

#[test]
fn functional_refresh_observes_backing_growth_before_append() {
    let server = TestServer::empty();
    server.seed("data", b"");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    std::fs::write(server.root().join("data"), b"abcdef").unwrap();
    let attr = store.refresh("/data").unwrap();
    store.write("/data", attr.stat.size, b"-new").unwrap();
    assert_eq!(server.backing("data"), b"abcdef-new");
}

#[test]
fn functional_sync_keeps_written_data_on_owner() {
    let server = TestServer::empty();
    server.seed("data", b"abcdef");
    let store = store(&server);
    store.ensure_listed("/").unwrap();
    store.write("/data", 0, b"synced").unwrap();
    store.fsync("/data", false).unwrap();
    assert_eq!(server.backing("data"), b"synced");
}
