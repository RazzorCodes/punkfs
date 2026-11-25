//! Blackbox tests: start the real `punkfs` node processes autonomously —
//! the same way `scripts/playground.sh` does — and let them drive several
//! scenarios as a test. These assert the endgame behaviour (a folder built
//! from several remote folders, changes on one visible in the others,
//! working as a normal folder). They may all fail: we are building the
//! implementation toward them.
//!
//! Each scenario is a whole-session run: spawn N real node binaries over
//! real backing dirs wired as peers, wait until they are up, perform the
//! operations the playground documents, and assert the merged view end to
//! end. No synthetic in-process mocks; the real binaries are the system
//! under test.

#[path = "../posix_contract/mod.rs"]
mod posix_contract;

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Whether a real FUSE mount is usable here (blackbox drives real nodes that
/// mount). Skip when it is not, so the suite neither fails nor leaves
/// half-mounted state on a machine without FUSE.
fn require_fuse() -> bool {
    if Path::new("/dev/fuse").exists()
        && std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("fusermount3").exists()))
            .unwrap_or(false)
    {
        return true;
    }
    eprintln!("skipping: /dev/fuse or fusermount3 unavailable");
    false
}

/// A running `punkfs` node: covers its own backing directory, mounts a FUSE
/// view of its own + peers' files, and serves its peers over grpc.
struct Node {
    child: Child,
    root: tempfile::TempDir,
    _logs: tempfile::TempDir,
}

impl Node {
    /// Start a node whose backing root is `root` (pre-seeded by the test),
    /// with `id`, listening on `addr`, peered with the given `(id, addr)`
    /// pairs (ids unique and distinct from `id`).
    fn start(
        id: &str,
        root: tempfile::TempDir,
        addr: SocketAddr,
        peers: &[(String, SocketAddr)],
    ) -> Self {
        Self::start_with_cache(id, root, addr, peers, None)
    }

    fn start_with_cache(
        id: &str,
        root: tempfile::TempDir,
        addr: SocketAddr,
        peers: &[(String, SocketAddr)],
        cache: Option<(u64, usize)>,
    ) -> Self {
        let logs = tempfile::tempdir().expect("separate log directory");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_punkfs"));
        if let Some((ttl_ms, max_entries)) = cache {
            let cfg = logs.path().join("cache.toml");
            std::fs::write(
                &cfg,
                format!("cache_ttl_ms = {ttl_ms}\ncache_max_entries = {max_entries}\n"),
            )
            .unwrap();
            cmd.arg("--config").arg(cfg);
        }
        cmd.arg(root.path())
            .arg(id)
            .arg("--listen")
            .arg(addr.to_string())
            .arg("--db-path")
            .arg(logs.path().join("catalog.redb"))
            .env("PUNKFS_LOG_DESTINATION", "file")
            .env("PUNKFS_LOG_LEVEL", "debug")
            .env("PUNKFS_LOG_FILE", logs.path().join("node.jsonl"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (peer_id, peer_addr) in peers {
            cmd.arg(format!("{peer_id}={}", url(*peer_addr)));
        }
        let child = cmd.spawn().expect("spawn punkfs node");
        let mut node = Self {
            child,
            root,
            _logs: logs,
        };
        node.wait_ready();
        node
    }

    /// The backing directory / FUSE mount point (the node covers its own
    /// root, so backing == mount view for local files).
    fn path(&self) -> &Path {
        self.root.path()
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let target = std::fs::canonicalize(self.root.path()).unwrap();
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                panic!("node exited during startup");
            }
            let mounted = std::fs::read_to_string("/proc/self/mountinfo")
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.split_once(" - "))
                .any(|(left, right)| {
                    left.split_whitespace().nth(4) == target.to_str()
                        && right
                            .split_whitespace()
                            .next()
                            .is_some_and(|kind| kind.starts_with("fuse"))
                });
            if mounted && std::fs::read_dir(&target).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("node never mounted at {}", target.display());
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(self.root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.stop();
        }
    }
}

fn url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

/// Reserve `n` distinct loopback ports.
fn addresses(n: usize) -> Vec<SocketAddr> {
    let listeners: Vec<_> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("bind"))
        .collect();
    listeners.iter().map(|s| s.local_addr().unwrap()).collect()
}

/// The `(id, addr)` wiring for the peers of node `i` in an `n`-node start.
fn peers(i: usize, n: usize, ids: &[String], addrs: &[SocketAddr]) -> Vec<(String, SocketAddr)> {
    (0..n)
        .filter(|j| *j != i)
        .map(|j| (ids[j].clone(), addrs[j]))
        .collect()
}

/// Exercise a remote-owned file through a real two-node process deployment.
fn run_posix(contract: fn(&Path), mode: u32) {
    if !require_fuse() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let addrs = addresses(2);
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("data"), b"abcdef").unwrap();
    std::fs::write(first.path().join("other"), b"other").unwrap();
    std::fs::set_permissions(
        first.path().join("data"),
        std::fs::Permissions::from_mode(mode),
    )
    .unwrap();
    let mut a = Node::start("a", first, addrs[0], &[("b".into(), addrs[1])]);
    let mut b = Node::start("b", second, addrs[1], &[("a".into(), addrs[0])]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::symlink_metadata(b.path().join("data")).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "remote file never became visible"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    contract(b.path());
    b.stop();
    a.stop();
}

macro_rules! posix_contracts {
    ($($name:ident => $contract:ident $(, mode = $mode:expr)?;)+) => {$ (
        #[test]
        fn $name() { posix_contracts!(@run $contract $(, $mode)?); }
    )+};
    (@run $contract:ident, $mode:expr) => { run_posix(posix_contract::$contract, $mode) };
    (@run $contract:ident) => { run_posix(posix_contract::$contract, 0o600) };
}

posix_contracts! {
    blackbox_remote_truncate => truncate;
    blackbox_remote_chmod => mode;
    blackbox_remote_nanosecond_timestamps => timestamps;
    blackbox_remote_epoch_timestamp_is_preserved => epoch_timestamp;
    blackbox_remote_pre_epoch_timestamp_and_omit => timestamp_before_epoch_and_omit;
    blackbox_open_handle_after_unlink => open_handle_unlink;
    blackbox_open_handle_after_rename => open_handle_rename;
    blackbox_open_handle_after_destination_replacement => open_handle_replaced_destination;
    blackbox_zero_mode_is_reported => permissions, mode = 0;
    blackbox_access_denies_mode_zero => access_checks, mode = 0;
    blackbox_access_denies_without_execute_bits => access_executable;
    blackbox_owner_can_retain_ownership => ownership;
    blackbox_hard_links_share_identity => hard_link;
    blackbox_local_hard_links_share_identity => local_hard_link;
    blackbox_fifo_is_a_fifo => fifo;
    blackbox_fifo_transfers_bytes => fifo_exchange;
    blackbox_user_extended_attributes_round_trip => xattr;
    blackbox_directory_extended_attributes_round_trip => directory_xattr;
    blackbox_xattr_flags_and_buffers_match_linux => xattr_flags_and_buffers;
    blackbox_list_extended_attributes => xattr_list;
    blackbox_posix_acl_round_trips => acl_roundtrip;
    blackbox_non_utf8_filename_round_trips => byte_filename;
    blackbox_new_files_and_directories_can_be_created => create_file_and_directory;
    blackbox_symlinks_follow_normal_rules => symlink_semantics;
    blackbox_concurrent_append_is_atomic => concurrent_append;
    blackbox_concurrent_exclusive_create_has_one_winner => concurrent_create_exclusive;
    blackbox_advisory_locks_exclude_other_open_files => advisory_lock;
    blackbox_fsync_and_fdatasync_are_usable => fsync_data;
    blackbox_mmap_reads_file_contents => mmap_read;
    blackbox_rename_nonempty_directory_preserves_both => rename_nonempty;
    blackbox_rename_noreplace_preserves_existing_file => rename_noreplace;
    blackbox_rename_exchange_swaps_files => rename_exchange;
}

#[test]
fn blackbox_cross_owner_exchange_is_refused_without_data_loss() {
    use std::os::unix::ffi::OsStrExt;
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("remote"), b"remote").unwrap();
    std::fs::write(second.path().join("local"), b"local").unwrap();
    let mut a = Node::start("a", first, addrs[0], &[("b".into(), addrs[1])]);
    let mut b = Node::start("b", second, addrs[1], &[("a".into(), addrs[0])]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(b.path().join("remote")).is_err() {
        assert!(Instant::now() < deadline, "remote file not visible");
        std::thread::sleep(Duration::from_millis(20));
    }
    let local = std::ffi::CString::new(b.path().join("local").as_os_str().as_bytes()).unwrap();
    let remote = std::ffi::CString::new(b.path().join("remote").as_os_str().as_bytes()).unwrap();
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
    assert_eq!(std::fs::read(b.path().join("local")).unwrap(), b"local");
    assert_eq!(std::fs::read(a.path().join("remote")).unwrap(), b"remote");
    b.stop();
    a.stop();
}

#[test]
fn blackbox_remote_exchange_directories_keeps_subtree_inodes() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for (name, value) in [
        ("exchange-left", b"left".as_slice()),
        ("exchange-right", b"right".as_slice()),
    ] {
        let path = first.path().join(name).join("nested");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("file"), value).unwrap();
    }
    let mut a = Node::start("a", first, addrs[0], &[("b".into(), addrs[1])]);
    let mut b = Node::start("b", second, addrs[1], &[("a".into(), addrs[0])]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(b.path().join("exchange-left/nested/file")).is_err() {
        assert!(Instant::now() < deadline, "directory not visible");
        std::thread::sleep(Duration::from_millis(20));
    }
    posix_contract::rename_exchange_directories(b.path());
    b.stop();
    a.stop();
}

#[test]
fn blackbox_remote_fifo_receives_owner_backing_bytes() {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let path = first.path().join("preseeded-fifo");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    // Pin the owner's original backing directory before its node covers it.
    let backing = std::fs::File::open(first.path()).unwrap();
    let mut a = Node::start("a", first, addrs[0], &[("b".into(), addrs[1])]);
    let mut b = Node::start("b", second, addrs[1], &[("a".into(), addrs[0])]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::symlink_metadata(b.path().join("preseeded-fifo")).is_err() {
        assert!(Instant::now() < deadline, "FIFO not published");
        std::thread::sleep(Duration::from_millis(20));
    }
    let owner = std::thread::spawn(move || {
        let path = format!("/proc/self/fd/{}/preseeded-fifo", backing.as_raw_fd());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
            {
                Ok(mut writer) => {
                    writer.write_all(b"message").unwrap();
                    break true;
                }
                Err(err)
                    if err.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => break false,
            }
        }
    });
    let result = std::panic::catch_unwind(|| posix_contract::fifo_read_preseeded(b.path()));
    let wrote = owner.join().unwrap();
    b.stop();
    a.stop();
    result.unwrap();
    assert!(
        wrote,
        "owner's backing writer must connect to remote reader"
    );
}

#[test]
fn blackbox_owner_ids_match_backing_even_when_gid_is_zero() {
    if !require_fuse() {
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let addrs = addresses(2);
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let data = first.path().join("data");
    std::fs::write(&data, b"abcdef").unwrap();
    let raw = std::ffi::CString::new(data.as_os_str().as_encoded_bytes()).unwrap();
    if unsafe { libc::chown(raw.as_ptr(), u32::MAX, 0) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EPERM) {
            eprintln!("skipping gid-zero ownership test: no permission to set backing gid 0");
            return;
        }
        panic!("set backing gid: {err}");
    }
    let expected = std::fs::metadata(data).unwrap();
    let mut a = Node::start("a", first, addrs[0], &[("b".into(), addrs[1])]);
    let mut b = Node::start("b", second, addrs[1], &[("a".into(), addrs[0])]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::symlink_metadata(b.path().join("data")).is_err() {
        assert!(
            Instant::now() < deadline,
            "remote file never became visible"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    posix_contract::uid_gid_match(b.path(), expected.uid(), expected.gid());
    b.stop();
    a.stop();
}

fn seeded_xattr_case(contract: fn(&Path)) {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("data"), b"abcdef").unwrap();
    if let Err(err) = posix_contract::seed_xattr(&first.path().join("data")) {
        if matches!(
            err.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::EPERM)
        ) {
            eprintln!("skipping xattr: backing does not support it: {err}");
            return;
        }
        panic!("seed xattr: {err}");
    }
    let mut a = Node::start("a", first, addrs[0], &[("b".into(), addrs[1])]);
    let mut b = Node::start("b", second, addrs[1], &[("a".into(), addrs[0])]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::symlink_metadata(b.path().join("data")).is_err() {
        assert!(
            Instant::now() < deadline,
            "remote file never became visible"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    contract(b.path());
    b.stop();
    a.stop();
}

#[test]
fn blackbox_existing_user_xattr_can_be_read_listed_and_removed() {
    seeded_xattr_case(posix_contract::seeded_xattr);
}
#[test]
fn blackbox_existing_user_xattr_can_be_listed() {
    seeded_xattr_case(posix_contract::seeded_xattr_list);
}
#[test]
fn blackbox_existing_user_xattr_can_be_removed() {
    seeded_xattr_case(posix_contract::seeded_xattr_remove);
}

#[test]
fn blackbox_append_after_another_mount_grows_file_uses_current_eof() {
    if !require_fuse() {
        return;
    }
    use std::io::Write;
    let addrs = addresses(2);
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("data"), b"").unwrap();
    let mut a = Node::start("a", first, addrs[0], &[("b".into(), addrs[1])]);
    let mut b = Node::start("b", second, addrs[1], &[("a".into(), addrs[0])]);
    let file = b.path().join("data");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::metadata(&file).is_ok_and(|metadata| metadata.len() == 0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "remote empty file must be visible"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .open(a.path().join("data"))
        .unwrap();
    writer.write_all(b"abcdef").unwrap();
    drop(writer);
    let mut appender = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    appender.write_all(b"-new").unwrap();
    drop(appender);
    assert_eq!(
        std::fs::read(a.path().join("data")).unwrap(),
        b"abcdef-new",
        "append must use the current owner EOF"
    );
    b.stop();
    a.stop();
}

/// The whole product under one assertion: a folder is built from several
/// remote folders, a file seeded on one node's backing directory appears in
/// every other node's mount, and a file created through one node's mount is
/// visible in the others. A change in one folder is present in them all.
#[test]
fn blackbox_changes_on_one_node_are_visible_in_all_others() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(3);
    let ids: Vec<String> = (0..3).map(|i| format!("node-{i}")).collect();
    let roots: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    // Seed each node's own pre-existing file before its process starts.
    for i in 0..3 {
        std::fs::write(
            roots[i].path().join(format!("own-{i}.txt")),
            format!("from {i}"),
        )
        .unwrap();
    }
    let mut nodes = Vec::new();
    for (i, root) in roots.into_iter().enumerate() {
        let p = peers(i, 3, &ids, &addrs);
        nodes.push(Node::start(&ids[i], root, addrs[i], &p));
    }
    std::thread::sleep(Duration::from_millis(600));

    // Every node's mount lists every node's seeded file.
    for i in 0..3 {
        for j in 0..3 {
            let content = std::fs::read_to_string(nodes[i].path().join(format!("own-{j}.txt")));
            assert!(
                content.is_ok() && content.unwrap().trim() == format!("from {j}"),
                "node {i}'s mount must show node {j}'s file"
            );
        }
    }

    // Create a file through node 0's mount; it must appear in the others.
    std::fs::write(
        nodes[0].path().join("new-file.txt"),
        b"created-through-mount",
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(900));
    for i in 0..3 {
        let ok = std::fs::read(nodes[i].path().join("new-file.txt"))
            .ok()
            .as_deref()
            == Some(b"created-through-mount".as_ref());
        assert!(ok, "change on node 0 must be visible in node {i}'s mount");
    }

    for node in nodes.iter_mut() {
        node.stop();
    }
}

/// The merged folder behaves like a normal folder for reading a remote
/// file: the bytes a peer holds resolve through the other node's mount.
#[test]
fn blackbox_merged_folder_serves_remote_reads() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let roots: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    std::fs::write(roots[0].path().join("data.txt"), b"0123456789").unwrap();
    let ids = ["a".to_string(), "b".to_string()];
    let mut nodes = Vec::new();
    for (i, root) in roots.into_iter().enumerate() {
        let p = peers(i, 2, &ids, &addrs);
        nodes.push(Node::start(&ids[i], root, addrs[i], &p));
    }
    std::thread::sleep(Duration::from_millis(600));

    let remote = nodes[1].path().join("data.txt");
    assert_eq!(
        std::fs::read(&remote).ok(),
        Some(b"0123456789".to_vec()),
        "node 1 must read node 0's remote file unchanged"
    );

    for node in nodes.iter_mut() {
        node.stop();
    }
}

/// **Known gap — expected to fail today.** The merged folder must act like a
/// normal folder for the mutation operations a shell reaches for daily:
/// truncating an existing file (`echo > file` / `truncate -s`) and changing
/// its mode (`chmod`). Today the underlying `setattr` is refused with
/// `ENOSYS` (see `tests/CONFORMANCE.md`), so this test fails — it is the
/// specification we are building toward, exercised on the real running node
/// processes, not a mock.
#[test]
fn blackbox_mutations_on_remote_existing_files() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let roots: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    std::fs::write(roots[0].path().join("data.txt"), b"0123456789").unwrap();
    let ids = ["a".to_string(), "b".to_string()];
    let mut nodes = Vec::new();
    for (i, root) in roots.into_iter().enumerate() {
        let p = peers(i, 2, &ids, &addrs);
        nodes.push(Node::start(&ids[i], root, addrs[i], &p));
    }
    std::thread::sleep(Duration::from_millis(600));

    // Truncate a remote existing file through node 1's mount.
    // `echo > file` opens with O_TRUNC; that must empty the owning file.
    let remote = nodes[1].path().join("data.txt");
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&remote)
        .unwrap_or_else(|err| {
            panic!("CONTRACT: truncating an existing remote file must succeed, got {err:?} (setattr refused with ENOSYS today)")
        });

    // The owning node's file must now be empty. Node 0 covers its own backing
    // as its mount, so reading node 0's mount path reads node 0's backing.
    let backing_now = std::fs::read_to_string(nodes[0].path().join("data.txt"));
    let empty = backing_now.as_ref().map(|s| s.is_empty()).unwrap_or(false);
    assert!(
        empty,
        "after truncate the owning file must be empty, got {backing_now:?}"
    );

    // chmod a remote existing file through node 1's mount.
    use std::os::unix::fs::PermissionsExt;
    let remote_meta = std::fs::metadata(&remote).expect("remote file readable");
    let before = remote_meta.permissions().mode();
    std::fs::set_permissions(&remote, std::fs::Permissions::from_mode(0o600)).unwrap_or_else(|err| {
        panic!("CONTRACT: chmod on a remote existing file must succeed, got {err:?} (setattr refused today)")
    });
    let after = std::fs::metadata(&remote).unwrap().permissions().mode();
    assert_eq!(
        after & 0o777,
        0o600,
        "CONTRACT: chmod must change the mode (was 0o{before:o})"
    );

    for node in nodes.iter_mut() {
        node.stop();
    }
}

#[test]
fn blackbox_startup_scans_more_than_one_peer_inventory_batch() {
    if !require_fuse() {
        return;
    }
    let addr = addresses(1)[0];
    let root = tempfile::tempdir().unwrap();
    for i in 0..4097 {
        std::fs::File::create(root.path().join(format!("file-{i}"))).unwrap();
    }
    // Do not enumerate the FUSE mount here: that exercises a separate, much
    // more expensive directory-refresh path. Wait for the actual startup-scan
    // event, then check its durable result after stopping the process.
    let logs = tempfile::tempdir().unwrap();
    let db_path = logs.path().join("catalog.redb");
    let log_path = logs.path().join("node.jsonl");
    let child = Command::new(env!("CARGO_BIN_EXE_punkfs"))
        .arg(root.path())
        .arg("large")
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--db-path")
        .arg(&db_path)
        .env("PUNKFS_LOG_DESTINATION", "file")
        .env("PUNKFS_LOG_FILE", &log_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut node = Node {
        child,
        root,
        _logs: logs,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            node.child.try_wait().unwrap().is_none(),
            "node exited during startup"
        );
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log.contains("startup local scan completed") && log.contains("node started") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "large startup scan did not complete"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    node.stop();
    let db = punkfs::db::persistent::PersistentDb::open(&db_path).unwrap();
    assert!(db.get_inode("/file-4096").unwrap().is_some());
}

#[test]
fn blackbox_minimal_cache_allows_creating_local_file() {
    if !require_fuse() {
        return;
    }
    let addr = addresses(1)[0];
    let root = tempfile::tempdir().unwrap();
    let node = Node::start_with_cache("one", root, addr, &[], Some((25, 1)));
    let path = node.path().join("created");
    std::fs::write(&path, b"hello").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(std::fs::read(&path).unwrap(), b"hello");
}

#[test]
fn blackbox_peer_rename_after_cache_eviction_preserves_inode_and_removes_old_name() {
    if !require_fuse() {
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let addrs = addresses(2);
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();
    std::fs::write(root_a.path().join("before"), b"payload").unwrap();
    let a = Node::start("a", root_a, addrs[0], &[("b".into(), addrs[1])]);
    let b = Node::start_with_cache(
        "b",
        root_b,
        addrs[1],
        &[("a".into(), addrs[0])],
        Some((25, 1)),
    );
    let old = b.path().join("before");
    let ino = std::fs::metadata(&old).unwrap().ino();
    std::thread::sleep(Duration::from_millis(60));
    std::fs::rename(a.path().join("before"), a.path().join("after")).unwrap();
    let new = b.path().join("after");
    assert_eq!(std::fs::metadata(&new).unwrap().ino(), ino);
    assert_eq!(std::fs::read(&new).unwrap(), b"payload");
    assert!(!old.exists());
    let names: Vec<_> = std::fs::read_dir(b.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("after")]);
}

#[test]
fn blackbox_toml_config_and_redb_persist_inodes_across_restart() {
    if !require_fuse() {
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let addrs = addresses(2);
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_a.path().join("seed.txt"), b"seeded-before-start").unwrap();

    let cfg_dir = tempfile::tempdir().unwrap();
    let db_a = cfg_dir.path().join("a.redb");
    let db_b = cfg_dir.path().join("b.redb");
    let toml_a = cfg_dir.path().join("a.toml");
    let toml_b = cfg_dir.path().join("b.toml");

    std::fs::write(
        &toml_a,
        format!(
            "mountpoint = \"{}\"\nnode_id = \"a\"\nlisten = \"{}\"\ndb_path = \"{}\"\ncache_ttl_ms = 500\ncache_max_entries = 64\n\n[peers]\nb = \"{}\"\n",
            dir_a.path().display(),
            addrs[0],
            db_a.display(),
            url(addrs[1])
        ),
    )
    .unwrap();
    std::fs::write(
        &toml_b,
        format!(
            "mountpoint = \"{}\"\nnode_id = \"b\"\nlisten = \"{}\"\ndb_path = \"{}\"\ncache_ttl_ms = 500\ncache_max_entries = 64\n\n[peers]\na = \"{}\"\n",
            dir_b.path().display(),
            addrs[1],
            db_b.display(),
            url(addrs[0])
        ),
    )
    .unwrap();

    let spawn_from_toml = |cfg: &Path, mount: &Path| -> Child {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_punkfs"));
        cmd.arg("--config")
            .arg(cfg)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("spawn punkfs from toml");
        let target = std::fs::canonicalize(mount).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            assert!(
                child.try_wait().unwrap().is_none(),
                "node exited during startup"
            );
            let mounted = std::fs::read_to_string("/proc/self/mounts")
                .unwrap_or_default()
                .lines()
                .any(|line| {
                    let mut fields = line.split_whitespace();
                    fields.nth(1).is_some_and(|p| p == target.to_string_lossy())
                        && fields.next().is_some_and(|kind| kind.starts_with("fuse"))
                });
            if mounted {
                return child;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("node never mounted at {}", target.display());
    };

    let mut child_a = spawn_from_toml(&toml_a, dir_a.path());
    let mut child_b = spawn_from_toml(&toml_b, dir_b.path());

    // Wait for handshake to make node a's startup-scanned file visible on node b.
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(dir_b.path().join("seed.txt")).is_err() {
        assert!(Instant::now() < deadline, "seed.txt not visible on peer b");
        std::thread::sleep(Duration::from_millis(30));
    }
    let ino_before = std::fs::metadata(dir_b.path().join("seed.txt"))
        .unwrap()
        .ino();
    assert!(db_a.exists());
    assert!(db_b.exists());

    // Stop node b and restart it with the same config/db_path: its local FUSE inode
    // for seed.txt must remain constant from redb.
    let _ = Command::new("kill")
        .args(["-TERM", &child_b.id().to_string()])
        .status();
    let _ = child_b.wait();
    // Stop the owner as well: the observer cannot rediscover the entry by RPC,
    // so an inode match here proves the persisted catalog supplied it.
    let _ = Command::new("kill")
        .args(["-TERM", &child_a.id().to_string()])
        .status();
    let _ = child_a.wait();

    let mut child_b2 = spawn_from_toml(&toml_b, dir_b.path());
    let ino_after = std::fs::metadata(dir_b.path().join("seed.txt"))
        .unwrap()
        .ino();
    assert_eq!(ino_before, ino_after, "constant local inode across restart");

    for (child, dir) in [(&mut child_b2, dir_b.path())] {
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        let _ = child.wait();
        let _ = Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The endgame cross-write path: create a file through node 0's mount,
/// rewrite/append it through node 1's mount, then read the result through
/// node 2's mount. A change written on one mount must be present and
/// viewable on another.
///
/// This is the strictest "one folder from several remote folders" claim: the
/// mutation happens on a *different* mount than the one that created the
/// file, and a *third* mount observes it. It may fail today where the
/// write-through-routing or metadata refresh is incomplete — that is the
/// specification we build toward.
#[test]
fn blackbox_cross_mount_write_is_visible_on_a_third_mount() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(3);
    let ids: Vec<String> = (0..3).map(|i| format!("node-{i}")).collect();
    let roots: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes = Vec::new();
    for (i, root) in roots.into_iter().enumerate() {
        let p = peers(i, 3, &ids, &addrs);
        nodes.push(Node::start(&ids[i], root, addrs[i], &p));
    }
    std::thread::sleep(Duration::from_millis(600));

    // 1) Create a file through node 0's mount.
    std::fs::write(nodes[0].path().join("cross.txt"), b"version-1").unwrap();

    // 2) Append to it through node 1's mount (node 1 does not own the file).
    //    Append avoids O_TRUNC so the write is not blocked by the setattr gap;
    //    this isolates the cross-mount *routing* being tested here.
    {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(nodes[1].path().join("cross.txt"))
            .expect("node 1 must open node 0's file for writing")
            .write_all(b"-v2")
            .expect("append through node 1's mount");
    }

    // 3) Read it through node 2's mount — the change written on mount 1 must
    //    be present and viewable on a third mount. Poll briefly, because
    //    under parallel load the change may need a listing refresh to arrive
    //    on node 2's view.
    let deadline = Instant::now() + Duration::from_secs(10);
    let content = loop {
        let c = std::fs::read(nodes[2].path().join("cross.txt"))
            .expect("node 2 must see the file created on node 0");
        if c == b"version-1-v2" || Instant::now() >= deadline {
            break c;
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    assert_eq!(
        content, b"version-1-v2",
        "the append made through node 1's mount must be visible on node 2's mount"
    );

    for node in nodes.iter_mut() {
        node.stop();
    }
}
