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
        let logs = tempfile::tempdir().expect("separate log directory");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_punkfs"));
        cmd.arg(root.path())
            .arg(id)
            .arg("--listen")
            .arg(addr.to_string())
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
    blackbox_user_extended_attributes_round_trip => xattr;
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
