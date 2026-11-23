//! Integration test entry point.
//!
//! A real FUSE mount over a real backing directory: the fixture must act
//! like our mocks. These tests assert POSIX behaviour through actual
//! syscalls on a mounted punkfs (`*_conformance`, `handle_conformance`, and
//! the FUSE portions of `node_tests`/`telemetry_tests`).
//!
//! The store-level and in-process server tests that do **not** need a FUSE
//! mount live in `tests/functional/`.

#[path = "../common/mod.rs"]
mod common;

mod cli_conformance;
mod handle_conformance;
mod local_passthrough_tests;
mod metadata_conformance;
mod multinode_conformance;
mod node_tests;
mod mutation_conformance;
mod read_conformance;
mod telemetry_tests;
mod write_conformance;

/// Guards the harness itself: a failing test must not leave an orphan server
/// process or a mounted filesystem behind.
mod harness_tests {
    use super::common;
    use std::process::Command;

    #[test]
    fn dropping_a_mount_unmounts() {
        if !common::require_fuse() {
            return;
        }
        let (server, mount) = common::mount_fixture();
        let path = mount.path().to_path_buf();
        assert!(std::fs::read_dir(&path).is_ok());

        drop(mount);

        let listed = Command::new("mount").output().unwrap();
        let mounts = String::from_utf8_lossy(&listed.stdout);
        assert!(
            !mounts.contains(path.to_str().unwrap()),
            "mount {} survived its guard",
            path.display()
        );
        drop(server);
    }

    #[test]
    fn dropping_a_server_kills_the_process() {
        let server = common::TestServer::empty();
        let addr = server.addr.clone();
        assert!(std::net::TcpStream::connect(&addr).is_ok());
        drop(server);

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            std::net::TcpStream::connect(&addr).is_err(),
            "server on {addr} survived its guard"
        );
    }
}
