use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::LazyLock;
use std::time::Duration;

pub struct TestServer {
    pub url: String,
    _child: Child,
    _stdin: ChildStdin,
}

pub static SERVER: LazyLock<TestServer> = LazyLock::new(|| {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = format!("127.0.0.1:{port}");
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/testdir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_punkfs-server"))
        .arg(root)
        .arg(&addr)
        .env("PUNKFS_EXIT_ON_STDIN_EOF", "1")
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn punkfs-server");
    let stdin = child.stdin.take().unwrap();

    // Plain blocking TCP probe, so no async runtime is needed in the static.
    for _ in 0..50 {
        if TcpStream::connect(&addr).is_ok() {
            return TestServer {
                url: format!("http://{addr}"),
                _child: child,
                _stdin: stdin,
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("server did not start on {addr}");
});
