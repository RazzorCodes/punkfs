//! Shared harness for the integration tests.
//!
//! Two layers of tests use this:
//!
//! - **Contract tests** assert what a mounted punkfs *must* do to be usable
//!   as a filesystem. They are written against POSIX expectations, not
//!   against current behaviour, so a passing suite means the mount is
//!   correct and a failing test names a real gap. Every test that is
//!   expected to fail today says so in its comment and in its assertion
//!   message, and the failure text states expected vs actual.
//! - **Regression tests** pin behaviour that already works.
//!
//! Every test gets its own `punkfs-server` on an ephemeral port and, when it
//! needs one, its own mount. Both are RAII guards: dropping them kills the
//! process and unmounts, so a failing test cannot leak into the next run.
//!
//! The server root is always a fresh `TempDir`, never `tests/testdir`, so a
//! mutation test cannot write into the source tree.
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for a freshly spawned server to accept connections.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for a mount to become usable.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(10);

/// The checked-in fixture, used by tests that want a pre-populated tree.
pub fn fixture_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/testdir"))
}

/// A `punkfs-server` child process, killed on drop.
pub struct TestServer {
    pub url: String,
    pub addr: String,
    pub root: tempfile::TempDir,
    child: std::sync::Mutex<Option<Child>>,
}

impl TestServer {
    /// Spawn a server over a fresh copy of `tests/testdir`.
    pub fn with_fixture() -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        copy_dir(&fixture_dir(), dir.path());
        Self::over(dir)
    }

    /// Spawn a server over an empty temporary directory.
    pub fn empty() -> Self {
        Self::over(tempfile::TempDir::new().expect("tempdir"))
    }

    /// Spawn a server over a tempdir it takes ownership of.
    pub fn over(root: tempfile::TempDir) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port();
        let addr = format!("127.0.0.1:{port}");

        let child = Command::new(env!("CARGO_BIN_EXE_punkfs-server"))
            .arg(root.path())
            .arg(&addr)
            .arg("test-node")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn punkfs-server");

        wait_for_port(&addr);
        TestServer {
            url: format!("http://{addr}"),
            addr,
            root,
            child: std::sync::Mutex::new(Some(child)),
        }
    }

    /// The directory the server is serving — the "backing store".
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Create a file in the backing store, bypassing punkfs.
    pub fn seed(&self, path: &str, contents: &[u8]) {
        let target = self.root().join(path.trim_start_matches('/'));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("seed parent");
        }
        std::fs::write(&target, contents).expect("seed file");
    }

    /// Create a directory in the backing store, bypassing punkfs.
    pub fn seed_dir(&self, path: &str) {
        std::fs::create_dir_all(self.root().join(path.trim_start_matches('/')))
            .expect("seed dir");
    }

    /// Read a file straight from the backing store.
    pub fn backing(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.root().join(path.trim_start_matches('/'))).expect("read backing")
    }

    /// Kill this server's process, simulating the machine going away.
    ///
    /// Idempotent, and safe to call through `&self` so a test can take a
    /// node down without moving the guard out of the cluster.
    pub fn stop(&self) {
        let mut guard = self.child.lock().unwrap();
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Whether the backing store contains `path`.
    pub fn backing_exists(&self, path: &str) -> bool {
        self.root().join(path.trim_start_matches('/')).exists()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A mounted punkfs, unmounted on drop.
pub struct Mount {
    pub path: tempfile::TempDir,
}

impl Mount {
    /// Mount `server` at a fresh temporary directory.
    pub fn new(server: &TestServer) -> Self {
        let path = tempfile::TempDir::new().expect("tempdir");
        let node = punkfs::FsNode {
            node_id: "test-node".into(),
        };

        let session = punkfs::PunkFs::mount(&server.url, path.path(), node)
            .unwrap_or_else(|e| panic!("mount failed: {e}"));

        // The session keeps the fs alive on background threads; leaking it
        // is intentional because the guard unmounts explicitly on drop.
        std::mem::forget(session);

        let mount = Mount { path };
        mount.wait_ready();
        mount
    }

    /// The mount point path.
    pub fn path(&self) -> &Path {
        self.path.path()
    }

    /// Path of an entry inside the mount.
    pub fn at(&self, rel: &str) -> PathBuf {
        self.path().join(rel.trim_start_matches('/'))
    }

    /// Poll until the mount answers, so a slow first RPC does not look like
    /// a bug in the test.
    fn wait_ready(&self) {
        let deadline = Instant::now() + MOUNT_TIMEOUT;
        while Instant::now() < deadline {
            if std::fs::read_dir(self.path()).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("mount at {} never became readable", self.path().display());
    }

    /// Unmount, retrying with `-z` when the fs is still busy.
    fn unmount(&self) {
        Mount::unmount_at(self.path.path());
    }

    /// Unmount `target`, tolerating an already-unmounted path.
    fn unmount_at(target: &Path) {
        for lazy in [true, false] {
            let mut cmd = Command::new("fusermount3");
            cmd.arg("-u");
            if lazy {
                cmd.arg("-z");
            }
            if matches!(cmd.arg(target).stdout(Stdio::null()).stderr(Stdio::null()).status(),
                        Ok(s) if s.success())
            {
                return;
            }
        }
        let _ = Command::new("fusermount")
            .args(["-u", "-z"])
            .arg(target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.unmount();
    }
}

/// A server plus a mount of it, for the common case.
///
/// Bind as `let (server, mount)` — dropping the server guard kills the
/// process and leaves the mount dangling.
pub fn mount_fixture() -> (TestServer, Mount) {
    let server = TestServer::with_fixture();
    let mount = Mount::new(&server);
    (server, mount)
}

/// A server plus a mount, both empty.
pub fn mount_empty() -> (TestServer, Mount) {
    let server = TestServer::empty();
    let mount = Mount::new(&server);
    (server, mount)
}

/// Several backing nodes merged into one mounted namespace.
///
/// Nodes are ordered and that order is the stable precedence used when two
/// nodes claim the same name, so tests can assert deterministic behaviour.
pub struct TestCluster {
    pub servers: Vec<TestServer>,
}

impl TestCluster {
    /// Start `n` independent servers, each over its own empty backing dir.
    pub fn empty(n: usize) -> Self {
        let servers: Vec<TestServer> = (0..n).map(|_| TestServer::empty()).collect();
        TestCluster::from_servers(servers)
    }

    pub fn from_servers(servers: Vec<TestServer>) -> Self {
        assert!(!servers.is_empty(), "cluster needs at least one node");
        TestCluster { servers }
    }

    /// `(node_id, url)` pairs for `Store::new_nodes`.
    ///
    /// `TestServer` always registers as `test-node`, and node ids must be
    /// unique, so they are qualified here; the backing dirs are what makes
    /// the nodes distinct.
    pub fn nodes(&self) -> Vec<(String, String)> {
        self.servers
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("node-{i}"), s.url.clone()))
            .collect()
    }

    pub fn node(&self, i: usize) -> &TestServer {
        &self.servers[i]
    }

    /// Stop node `i`, leaving the cluster's other nodes running.
    ///
    /// Simulates a backing machine going away mid-session. The node's
    /// entry stays in `servers`, so its `Drop` remains a harmless no-op.
    pub fn stop(&self, i: usize) {
        self.servers[i].stop();
    }
}

/// A mount backed by several nodes.
pub struct MultiMount {
    pub path: tempfile::TempDir,
}

impl MultiMount {
    pub fn new(cluster: &TestCluster) -> Self {
        let path = tempfile::TempDir::new().expect("tempdir");
        let node = punkfs::FsNode {
            node_id: "test-node".into(),
        };

        let session = punkfs::PunkFs::mount_cluster(&cluster.nodes(), path.path(), node)
            .unwrap_or_else(|e| panic!("mount failed: {e}"));
        std::mem::forget(session);

        let mount = MultiMount { path };
        mount.wait_ready();
        mount
    }

    pub fn path(&self) -> &Path {
        self.path.path()
    }

    pub fn at(&self, rel: &str) -> PathBuf {
        self.path().join(rel.trim_start_matches('/'))
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + MOUNT_TIMEOUT;
        while Instant::now() < deadline {
            if std::fs::read_dir(self.path()).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("mount at {} never became readable", self.path().display());
    }
}

impl Drop for MultiMount {
    fn drop(&mut self) {
        Mount::unmount_at(self.path.path());
    }
}

/// Whether FUSE is usable here.
pub fn fuse_available() -> bool {
    if !Path::new("/dev/fuse").exists() {
        return false;
    }
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|dir| dir.join("fusermount3").exists())
        })
        .unwrap_or(false)
}

/// Skip the current test when FUSE is unusable, instead of failing.
pub fn require_fuse() -> bool {
    if fuse_available() {
        return true;
    }
    eprintln!("skipping: /dev/fuse or fusermount3 unavailable");
    false
}

/// Assert that a filesystem operation fails with a specific errno.
///
/// Takes the `io::ErrorKind` a caller observes, because that is the only
/// thing a portable test can see, and prints the raw error when it differs.
///
/// The message is deliberately verbose: a failing conformance test should
/// explain the contract it checked, not just report two different enums.
#[macro_export]
macro_rules! assert_errno {
    ($result:expr, $expected:expr, $contract:expr) => {{
        match $result {
            Ok(value) => panic!(
                "CONTRACT VIOLATED\n  expected: error {:?}\n  contract: {}\n  got:      success ({:?})",
                $expected, $contract, value
            ),
            Err(err) => {
                let kind = err.kind();
                assert_eq!(
                    kind, $expected,
                    "\nCONTRACT VIOLATED\n  expected: {:?}\n  contract: {}\n  got:      {:?}\n  raw: {}",
                    $expected,
                    $contract,
                    kind,
                    err
                );
            }
        }
    }};
    ($result:expr, $expected:expr) => {
        $crate::assert_errno!($result, $expected, "")
    };
}

fn wait_for_port(addr: &str) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start on {addr}");
}

/// Recursively copy `from` into `to`.
fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        let file_type = entry.file_type().expect("file type");
        if file_type.is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}
