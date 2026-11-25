//! Store-level peer behaviour: the client `Store` over several live
//! `punkfs-server` processes, and the grpc backing's symlink handling.
//! Whole operations exercised against the out-of-process mock system, no
//! FUSE mount.

use punkfs::FsNode;
use punkfs::Store;
use punkfs::StoreApi;
use std::fs;
use std::net::TcpListener;
use std::time::Duration;

use crate::common::TestServer;

/// Unresponsive peers must not serially block a healthy listing: the store
/// talks to nodes concurrently, so one silent peer is not a stall.
#[test]
fn functional_unresponsive_peers_do_not_block_a_healthy_listing() {
    let healthy = TestServer::empty();
    healthy.seed("online.txt", b"online");
    let mut nodes = vec![("healthy".to_string(), healthy.url.clone())];
    let mut stop = Vec::new();
    for i in 0..3 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        nodes.push((
            format!("silent-{i}"),
            format!("http://{}", listener.local_addr().unwrap()),
        ));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let (_connection, _) = listener.accept().unwrap();
            let _ = rx.recv_timeout(Duration::from_secs(8));
        });
        stop.push((tx, thread));
    }
    let store = Store::new_nodes(
        &nodes,
        FsNode {
            node_id: "reader".into(),
        },
    )
    .unwrap();
    let started = std::time::Instant::now();
    let result = store.ensure_listed_in(&punkfs::telemetry::OperationContext::new(), "/");
    let elapsed = started.elapsed();
    for (tx, thread) in stop {
        let _ = tx.send(());
        thread.join().unwrap();
    }
    result.unwrap();
    assert!(store.get_inode("/online.txt").is_some());
    assert!(
        elapsed < Duration::from_secs(4),
        "listing blocked for {elapsed:?}"
    );
}

/// The backing service exposes link metadata but never follows a link into
/// another directory when servicing content operations.
#[test]
fn functional_grpc_backing_exposes_symlinks_without_following_them() {
    use punkfs::grpc::service::PunkFsService;
    use punkfs::punk_fs::punk_fs_server::PunkFs as _;
    use punkfs::punk_fs::{
        FileKind, GetAttrRequest, ListDirectoryRequest, ReadFileRequest, ReadLinkRequest,
        SetAttrRequest, WriteFileRequest,
    };

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("private"), b"unchanged").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("private"), root.path().join("link")).unwrap();
    let service = PunkFsService::new(root.path(), "node");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let list = service
            .list_directory(tonic::Request::new(ListDirectoryRequest {
                path: "/".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        for name in ["escape", "link"] {
            assert_eq!(
                list.entries
                    .iter()
                    .find(|entry| entry.name == name)
                    .unwrap()
                    .attr
                    .as_ref()
                    .unwrap()
                    .kind,
                FileKind::Symlink as i32
            );
        }
        let attr = service
            .get_attr(tonic::Request::new(GetAttrRequest {
                path: "/link".into(),
            }))
            .await
            .unwrap()
            .into_inner()
            .attr
            .unwrap();
        assert_eq!(attr.kind, FileKind::Symlink as i32);
        use std::os::unix::ffi::OsStrExt;
        let link = service
            .read_link(tonic::Request::new(ReadLinkRequest {
                path: "/link".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            link.target,
            outside.path().join("private").as_os_str().as_bytes()
        );
        assert_eq!(
            service
                .list_directory(tonic::Request::new(ListDirectoryRequest {
                    path: "/escape".into()
                }))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
        assert_eq!(
            service
                .read_file(tonic::Request::new(ReadFileRequest {
                    path: "/escape/private".into(),
                    offset: 0,
                    length: 64
                }))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
        assert_eq!(
            service
                .write_file(tonic::Request::new(WriteFileRequest {
                    path: "/escape/private".into(),
                    offset: 0,
                    data: b"changed".to_vec()
                }))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
        assert_eq!(
            service
                .set_attr(tonic::Request::new(SetAttrRequest {
                    path: "/link".into(),
                    size: Some(0),
                    ..Default::default()
                }))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
    });
    assert_eq!(
        fs::read(outside.path().join("private")).unwrap(),
        b"unchanged"
    );
}
