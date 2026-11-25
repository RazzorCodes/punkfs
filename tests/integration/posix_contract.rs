//! Desired POSIX contracts through a real combined FUSE mount.
use crate::{common, posix_contract};
use punkfs::{FsNode, PunkFs};
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn run(contract: fn(&std::path::Path), mode: u32) {
    if !common::require_fuse() {
        return;
    }
    let server = common::TestServer::empty();
    server.seed("data", b"abcdef");
    server.seed("other", b"other");
    fs::set_permissions(server.root().join("data"), fs::Permissions::from_mode(mode)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = PunkFs::mount_with_local(
        &[("remote".into(), server.url.clone())],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
    )
    .unwrap();
    contract(root.path());
    drop(session);
}

macro_rules! contracts {
    ($($name:ident => $contract:ident $(, mode = $mode:expr)?;)+) => {$ (
        #[test]
        fn $name() { contracts!(@run $contract $(, $mode)?); }
    )+};
    (@run $contract:ident, $mode:expr) => { run(posix_contract::$contract, $mode) };
    (@run $contract:ident) => { run(posix_contract::$contract, 0o600) };
}

#[test]
fn remote_fifo_transfers_bytes_from_owner() {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    if !common::require_fuse() {
        return;
    }
    let server = common::TestServer::empty();
    let path = server.root().join("preseeded-fifo");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let root = tempfile::tempdir().unwrap();
    let session = PunkFs::mount_with_local(
        &[("remote".into(), server.url.clone())],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
    )
    .unwrap();
    let owner = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
            {
                Ok(mut writer) => {
                    writer.write_all(b"message").unwrap();
                    break true;
                }
                Err(err)
                    if err.raw_os_error() == Some(libc::ENXIO)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(_) => break false,
            }
        }
    });
    let result = std::panic::catch_unwind(|| posix_contract::fifo_read_preseeded(root.path()));
    let wrote = owner.join().unwrap();
    drop(session);
    result.unwrap();
    assert!(wrote, "backing writer must connect to mount reader");
}

#[test]
fn cross_owner_exchange_is_refused_without_data_loss() {
    use std::os::unix::ffi::OsStrExt;
    if !common::require_fuse() {
        return;
    }
    let server = common::TestServer::empty();
    server.seed("remote", b"remote");
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("local"), b"local").unwrap();
    let session = PunkFs::mount_with_local(
        &[("remote-node".into(), server.url.clone())],
        root.path(),
        FsNode {
            node_id: "local-node".into(),
        },
    )
    .unwrap();
    let local = std::ffi::CString::new(root.path().join("local").as_os_str().as_bytes()).unwrap();
    let remote = std::ffi::CString::new(root.path().join("remote").as_os_str().as_bytes()).unwrap();
    let _ = fs::metadata(root.path().join("local")).unwrap();
    let _ = fs::metadata(root.path().join("remote")).unwrap();
    assert_eq!(
        unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                local.as_ptr(),
                libc::AT_FDCWD,
                remote.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EXDEV)
    );
    assert_eq!(fs::read(root.path().join("local")).unwrap(), b"local");
    assert_eq!(server.backing("remote"), b"remote");
    drop(session);
}

#[test]
fn remote_exchange_directories_keeps_subtree_inodes() {
    if !common::require_fuse() {
        return;
    }
    let server = common::TestServer::empty();
    server.seed("exchange-left/nested/file", b"left");
    server.seed("exchange-right/nested/file", b"right");
    let root = tempfile::tempdir().unwrap();
    let session = PunkFs::mount_with_local(
        &[("remote".into(), server.url.clone())],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
    )
    .unwrap();
    posix_contract::rename_exchange_directories(root.path());
    drop(session);
}

#[test]
fn local_exchange_preserves_inode_identity() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    if !common::require_fuse() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("left"), b"left").unwrap();
    fs::write(root.path().join("right"), b"right").unwrap();
    let session = PunkFs::mount_with_local(
        &[],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
    )
    .unwrap();
    let left = root.path().join("left");
    let right = root.path().join("right");
    let left_ino = fs::metadata(&left).unwrap().ino();
    let right_ino = fs::metadata(&right).unwrap().ino();
    let left_c = std::ffi::CString::new(left.as_os_str().as_bytes()).unwrap();
    let right_c = std::ffi::CString::new(right.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                left_c.as_ptr(),
                libc::AT_FDCWD,
                right_c.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        },
        0
    );
    assert_eq!(fs::read(&left).unwrap(), b"right");
    assert_eq!(fs::read(&right).unwrap(), b"left");
    assert_eq!(fs::metadata(&left).unwrap().ino(), right_ino);
    assert_eq!(fs::metadata(&right).unwrap().ino(), left_ino);
    drop(session);
}

#[test]
fn client_only_mount_can_create_regular_files_and_directories() {
    if !common::require_fuse() {
        return;
    }
    let (_server, mount) = common::mount_empty();
    posix_contract::create_file_and_directory(mount.path());
}

#[test]
fn client_only_mount_can_create_symlinks() {
    if !common::require_fuse() {
        return;
    }
    let (server, mount) = common::mount_empty();
    server.seed("data", b"abcdef");
    posix_contract::symlink_semantics(mount.path());
}

#[test]
fn owner_ids_match_backing_even_when_gid_is_zero() {
    use std::os::unix::fs::MetadataExt;
    if !common::require_fuse() {
        return;
    }
    let server = common::TestServer::empty();
    server.seed("data", b"abcdef");
    let path =
        std::ffi::CString::new(server.root().join("data").as_os_str().as_encoded_bytes()).unwrap();
    // Changing the group to root is allowed only when the test user belongs to
    // that group or has CAP_CHOWN; skip only this subcase when unavailable.
    if unsafe { libc::chown(path.as_ptr(), u32::MAX, 0) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EPERM) {
            eprintln!("skipping gid-zero ownership test: no permission to set backing gid 0");
            return;
        }
        panic!("set backing gid: {err}");
    }
    let expected = fs::metadata(server.root().join("data")).unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = PunkFs::mount_with_local(
        &[("remote".into(), server.url.clone())],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
    )
    .unwrap();
    posix_contract::uid_gid_match(root.path(), expected.uid(), expected.gid());
    drop(session);
}

fn seeded_xattr_case(contract: fn(&std::path::Path)) {
    if !common::require_fuse() {
        return;
    }
    let server = common::TestServer::empty();
    server.seed("data", b"abcdef");
    if let Err(err) = posix_contract::seed_xattr(&server.root().join("data")) {
        if matches!(
            err.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::EPERM)
        ) {
            eprintln!("skipping xattr: backing does not support it: {err}");
            return;
        }
        panic!("seed xattr: {err}");
    }
    let root = tempfile::tempdir().unwrap();
    let session = PunkFs::mount_with_local(
        &[("remote".into(), server.url.clone())],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
    )
    .unwrap();
    contract(root.path());
    drop(session);
}

#[test]
fn existing_user_xattr_can_be_read_listed_and_removed() {
    seeded_xattr_case(posix_contract::seeded_xattr);
}
#[test]
fn existing_user_xattr_can_be_listed() {
    seeded_xattr_case(posix_contract::seeded_xattr_list);
}
#[test]
fn existing_user_xattr_can_be_removed() {
    seeded_xattr_case(posix_contract::seeded_xattr_remove);
}

#[test]
fn append_after_backing_growth_uses_current_eof() {
    use std::io::Write;
    if !common::require_fuse() {
        return;
    }
    let server = common::TestServer::empty();
    server.seed("data", b"");
    let root = tempfile::tempdir().unwrap();
    let session = PunkFs::mount_with_local(
        &[("remote".into(), server.url.clone())],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
    )
    .unwrap();
    assert_eq!(fs::metadata(root.path().join("data")).unwrap().len(), 0);
    fs::write(server.root().join("data"), b"abcdef").unwrap();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(root.path().join("data"))
        .unwrap();
    file.write_all(b"-new").unwrap();
    drop(file);
    assert_eq!(
        server.backing("data"),
        b"abcdef-new",
        "append must use the current owner EOF"
    );
    drop(session);
}

contracts! {
    remote_truncate => truncate;
    remote_chmod => mode;
    remote_nanosecond_timestamps => timestamps;
    remote_epoch_timestamp_is_preserved => epoch_timestamp;
    remote_pre_epoch_timestamp_and_omit => timestamp_before_epoch_and_omit;
    open_handle_after_remote_unlink => open_handle_unlink;
    open_handle_after_remote_rename => open_handle_rename;
    open_handle_after_destination_replacement => open_handle_replaced_destination;
    zero_mode_is_reported => permissions, mode = 0;
    access_denies_mode_zero => access_checks, mode = 0;
    access_denies_without_execute_bits => access_executable;
    owner_can_retain_ownership => ownership;
    hard_links_share_identity => hard_link;
    local_hard_links_share_identity => local_hard_link;
    fifo_is_a_fifo => fifo;
    fifo_transfers_bytes => fifo_exchange;
    user_extended_attributes_round_trip => xattr;
    directory_extended_attributes_round_trip => directory_xattr;
    xattr_flags_and_buffers_match_linux => xattr_flags_and_buffers;
    list_extended_attributes => xattr_list;
    posix_acl_round_trips => acl_roundtrip;
    non_utf8_filename_round_trips => byte_filename;
    new_files_and_directories_can_be_created => create_file_and_directory;
    symlinks_follow_normal_rules => symlink_semantics;
    concurrent_append_is_atomic => concurrent_append;
    concurrent_exclusive_create_has_one_winner => concurrent_create_exclusive;
    advisory_locks_exclude_other_open_files => advisory_lock;
    fsync_and_fdatasync_are_usable => fsync_data;
    mmap_reads_file_contents => mmap_read;
    rename_nonempty_directory_preserves_both => rename_nonempty;
    rename_noreplace_preserves_existing_file => rename_noreplace;
    rename_exchange_swaps_files => rename_exchange;
}
