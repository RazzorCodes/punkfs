//! Real processes, listeners and FUSE mounts for bidirectional node mode.
use super::common::require_fuse;
use punkfs::punk_fs::punk_fs_client::PunkFsClient;
use punkfs::punk_fs::{ListDirectoryRequest, ReadFileRequest, ShutdownRequest};
use punkfs::telemetry::{CORRELATION_HEADER, OperationContext};
use std::fs::{self, File};
use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn addresses(n: usize) -> Vec<SocketAddr> {
    let listeners: Vec<_> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners.iter().map(|s| s.local_addr().unwrap()).collect()
}

struct Node {
    child: Child,
    root: tempfile::TempDir,
    backing: File,
    logs: tempfile::TempDir,
    addr: SocketAddr,
}
impl Node {
    fn start(id: &str, addr: SocketAddr, peers: &[(&str, SocketAddr)], file: &str) -> Self {
        Self::start_with_level(id, addr, peers, file, "debug")
    }
    fn start_with_level(
        id: &str,
        addr: SocketAddr,
        peers: &[(&str, SocketAddr)],
        file: &str,
        level: &str,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(file), id.as_bytes()).unwrap();
        let backing = File::open(root.path()).unwrap();
        let logs = tempfile::tempdir().unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_punkfs"));
        cmd.arg(root.path())
            .arg(id)
            .arg("--listen")
            .arg(addr.to_string())
            .env("PUNKFS_LOG_DESTINATION", "file")
            .env("PUNKFS_LOG_LEVEL", level)
            .env("PUNKFS_LOG_FILE", logs.path().join("node.jsonl"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (peer, address) in peers {
            cmd.arg(format!("{peer}=http://{address}"));
        }
        let child = cmd.spawn().unwrap();
        let mut node = Self {
            child,
            root,
            backing,
            logs,
            addr,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let rt = tokio::runtime::Runtime::new().unwrap();
        loop {
            assert!(
                node.child.try_wait().unwrap().is_none(),
                "node exited during startup"
            );
            let result = rt.block_on(async {
                let client = PunkFsClient::connect(format!("http://{}", node.addr)).await;
                match client {
                    Ok(mut client) => client
                        .list_directory(ListDirectoryRequest { path: "/".into() })
                        .await
                        .is_ok(),
                    Err(_) => false,
                }
            });
            if result {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "node service did not become ready"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        node
    }
    fn path(&self) -> &Path {
        self.root.path()
    }
    fn raw(&self, path: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.backing.as_raw_fd())).join(path)
    }
    fn stop(&mut self) -> std::process::ExitStatus {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "node did not stop after SIGTERM");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(self.root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[test]
fn nodes_serve_only_their_own_backing_and_mount_bidirectionally() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let mut a = Node::start("a", addrs[0], &[("b", addrs[1])], "a.txt");
    // A serves local files before B starts.
    assert_eq!(fs::read(a.path().join("a.txt")).unwrap(), b"a");
    let mut b = Node::start("b", addrs[1], &[("a", addrs[0])], "b.txt");
    assert_eq!(fs::read(a.path().join("b.txt")).unwrap(), b"b");
    assert_eq!(fs::read(b.path().join("a.txt")).unwrap(), b"a");
    fs::write(a.path().join("new.txt"), b"owned by a").unwrap();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(b.path().join("a.txt"))
        .unwrap();
    file.write_all(b"-remote").unwrap();
    drop(file);
    assert_eq!(fs::read(a.raw("a.txt")).unwrap(), b"a-remote");
    assert!(!b.raw("a.txt").exists());
    assert_eq!(fs::read(b.path().join("new.txt")).unwrap(), b"owned by a");
    assert!(!b.raw("new.txt").exists());

    let rt = tokio::runtime::Runtime::new().unwrap();
    let read_context = OperationContext::new();
    rt.block_on(async {
        let mut client = PunkFsClient::connect(format!("http://{}", a.addr))
            .await
            .unwrap();
        let ctx = OperationContext::new();
        let response = client
            .list_directory(ctx.request(ListDirectoryRequest { path: "/".into() }))
            .await
            .unwrap();
        assert_eq!(
            response
                .metadata()
                .get(CORRELATION_HEADER)
                .unwrap()
                .to_str()
                .unwrap(),
            ctx.id().to_string()
        );
        let names: Vec<_> = response
            .into_inner()
            .entries
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&"a.txt".into()));
        assert!(names.contains(&"new.txt".into()));
        assert!(!names.contains(&"b.txt".into()));
        let error = client
            .read_file(read_context.request(ReadFileRequest {
                path: "/b.txt".into(),
                offset: 0,
                length: 100,
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::NotFound);
        assert_eq!(
            client
                .shutdown(ShutdownRequest {})
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unimplemented
        );
    });
    assert!(b.stop().success());
    assert!(a.stop().success());
    let logs = fs::read_to_string(a.logs.path().join("node.jsonl")).unwrap();
    let events: Vec<serde_json::Value> = logs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .any(|event| event["component"] == "grpc.backing"
                && event["operation"] == "read"
                && event["severity"] == "warning"
                && event["correlation_id"] == read_context.id().to_string()
                && event["fields"]["node_id"] == "a")
    );
    assert!(!logs.contains("/b.txt"));
    assert!(!logs.contains("owned by a"));
    assert_eq!(fs::read(a.path().join("new.txt")).unwrap(), b"owned by a");
    assert!(!a.path().join("b.txt").exists());
}

fn wait_event(node: &Node, operation: &str, message: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = fs::read_to_string(node.logs.path().join("node.jsonl")) {
            for line in text.lines() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
                    && value["component"] == "peer.metadata"
                    && value["operation"] == operation
                    && value["message"] == message
                {
                    return value;
                }
            }
        }
        assert!(Instant::now() < deadline, "no {operation} event {message}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn startup_handshake_populates_cache_without_browsing_the_peer() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let mut a = Node::start("a", addrs[0], &[("b", addrs[1])], "a.txt");
    let mut b = Node::start("b", addrs[1], &[("a", addrs[0])], "b.txt");
    // No FUSE access to A has listed B before this event.
    let _ = wait_event(&b, "handshake", "peer handshake completed");
    assert!(b.stop().success());
    let names: Vec<_> = fs::read_dir(a.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        names.iter().any(|name| name == "b.txt"),
        "handshake must populate the offline peer cache"
    );
    assert!(a.stop().success());
}

#[test]
fn creation_is_published_with_the_fuse_correlation_id() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let mut a = Node::start("a", addrs[0], &[("b", addrs[1])], "a.txt");
    let mut b = Node::start("b", addrs[1], &[("a", addrs[0])], "b.txt");
    let _ = wait_event(&b, "handshake", "peer handshake completed");
    fs::write(a.path().join("published.txt"), b"private payload").unwrap();
    let accepted = wait_event(&b, "publish_create", "creation metadata accepted");
    let correlation = accepted["correlation_id"].as_str().unwrap().to_string();
    let _ = wait_event(&a, "publish_create", "creation metadata published");
    assert!(a.stop().success());
    let a_log = fs::read_to_string(a.logs.path().join("node.jsonl")).unwrap();
    let events: Vec<serde_json::Value> = a_log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .any(|e| e["component"] == "filesystem.passthrough"
                && e["operation"] == "create"
                && e["correlation_id"] == correlation)
    );
    assert!(events.iter().any(|e| e["component"] == "peer.metadata"
        && e["operation"] == "publish_create"
        && e["correlation_id"] == correlation));
    assert!(!a_log.contains("private payload"));
    let names: Vec<_> = fs::read_dir(b.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        names.iter().any(|name| name == "published.txt"),
        "create notification should reserve name after owner goes offline"
    );
    assert!(b.stop().success());
}

#[test]
fn publication_failure_is_correlated_and_info_threshold_filters_debug() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let mut a = Node::start_with_level("a", addrs[0], &[("b", addrs[1])], "a.txt", "info");
    let handshake = wait_event(&a, "handshake", "peer handshake failed; retrying");
    assert_eq!(handshake["severity"], "warning");
    fs::write(a.path().join("private-name"), b"private content").unwrap();
    let failed = wait_event(&a, "publish_create", "creation metadata publication failed");
    assert_eq!(failed["severity"], "warning");
    assert!(failed["correlation_id"].as_str().is_some());
    assert!(a.stop().success());
    let log = fs::read_to_string(a.logs.path().join("node.jsonl")).unwrap();
    assert!(!log.contains("private-name"));
    assert!(!log.contains("private content"));
    let events: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        !events
            .iter()
            .any(|event| event["severity"] == "debug" || event["severity"] == "trace")
    );
}

#[test]
fn only_file_creation_is_published_and_parent_is_included() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let mut a = Node::start("a", addrs[0], &[("b", addrs[1])], "a.txt");
    let mut b = Node::start("b", addrs[1], &[("a", addrs[0])], "b.txt");
    let _ = wait_event(&b, "handshake", "peer handshake completed");
    fs::create_dir(a.path().join("dir")).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(a.path().join("a.txt"))
        .unwrap()
        .write_all(b"-modified")
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let log = fs::read_to_string(b.logs.path().join("node.jsonl")).unwrap();
    assert!(
        !log.contains("creation metadata accepted"),
        "mkdir and writes must not publish"
    );
    fs::write(a.path().join("dir/new"), b"body").unwrap();
    let _ = wait_event(&b, "publish_create", "creation metadata accepted");
    let _ = wait_event(&a, "publish_create", "creation metadata published");
    assert!(a.stop().success());
    let names: Vec<_> = fs::read_dir(b.path().join("dir"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(names.iter().any(|name| name == "new"));
    assert!(b.stop().success());
}

#[test]
fn node_refuses_an_already_covered_directory() {
    if !require_fuse() {
        return;
    }
    let addrs = addresses(2);
    let mut node = Node::start("a", addrs[0], &[], "a.txt");
    let output = Command::new(env!("CARGO_BIN_EXE_punkfs"))
        .arg(node.path())
        .arg("other")
        .arg("--listen")
        .arg(addrs[1].to_string())
        .env("PUNKFS_LOG_DESTINATION", "stderr")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already on a FUSE filesystem"));
    assert_eq!(fs::read(node.path().join("a.txt")).unwrap(), b"a");
    assert!(node.stop().success());
}

#[test]
fn bind_failure_does_not_cover_the_backing_directory() {
    if !require_fuse() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("original"), b"safe").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_punkfs"))
        .arg(root.path())
        .arg("a")
        .arg("--listen")
        .arg(listener.local_addr().unwrap().to_string())
        .env("PUNKFS_LOG_DESTINATION", "stderr")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read(root.path().join("original")).unwrap(), b"safe");
    assert!(
        !Command::new("mountpoint")
            .arg("-q")
            .arg(root.path())
            .status()
            .unwrap()
            .success()
    );
}
