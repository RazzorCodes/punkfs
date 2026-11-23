//! Low-level contracts through the public store and backing-service APIs.
//! These exercise behaviors accessible below FUSE; absent store operations
//! are recorded as explicit uncovered gaps rather than fake passing tests.
use crate::common::TestServer;
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
                ..Default::default()
            },
        )
        .expect("backend utimens");
    assert_eq!(
        std::fs::metadata(server.root().join("data"))
            .unwrap()
            .mtime(),
        1_000_001
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
    store
        .rename("/data", "/other", fuser::RenameFlags::RENAME_EXCHANGE)
        .expect("same-owner rename exchange");
    assert_eq!(server.backing("data"), b"other");
    assert_eq!(server.backing("other"), b"abcdef");
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
