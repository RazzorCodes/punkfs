//! Correlation tests use synthetic interceptors, not application log events.
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use punkfs::grpc::service::PunkFsService;
use punkfs::punk_fs::ListDirectoryRequest;
use punkfs::punk_fs::punk_fs_client::PunkFsClient;
use punkfs::punk_fs::punk_fs_server::PunkFsServer;
use punkfs::telemetry::{CORRELATION_HEADER, OperationContext};
use punkfs::{FsNode, Store, StoreApi};
use tonic::{Request, Status};

struct ObservedServer {
    root: tempfile::TempDir,
    url: String,
    requests: Arc<Mutex<Vec<Option<String>>>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ObservedServer {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("hello.txt"), b"hello").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let service = PunkFsService::new(root.path(), "test-node");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let (stop, rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let interceptor = move |req: Request<()>| -> Result<Request<()>, Status> {
                    let id = req
                        .metadata()
                        .get(CORRELATION_HEADER)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    observed.lock().unwrap().push(id);
                    Ok(req)
                };
                let svc = tonic::service::interceptor::InterceptedService::new(
                    PunkFsServer::from_arc(service),
                    interceptor,
                );
                tonic::transport::Server::builder()
                    .add_service(svc)
                    .serve_with_shutdown(address, async {
                        let _ = rx.await;
                    })
                    .await
                    .unwrap();
            });
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(address).is_err() {
            assert!(Instant::now() < deadline, "server failed to start");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            root,
            url: format!("http://{address}"),
            requests,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
    fn ids(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(Clone::clone)
            .collect()
    }
}
impl Drop for ObservedServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn grpc_ingress_validates_and_preserves_correlation() {
    let server = ObservedServer::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut client = PunkFsClient::connect(server.url.clone()).await.unwrap();
        let context = OperationContext::new();
        let response = client
            .list_directory(context.request(ListDirectoryRequest { path: "/".into() }))
            .await
            .unwrap();
        assert_eq!(
            response
                .metadata()
                .get(CORRELATION_HEADER)
                .unwrap()
                .to_str()
                .unwrap(),
            context.id().to_string()
        );
        let response = client
            .list_directory(ListDirectoryRequest { path: "/".into() })
            .await
            .unwrap();
        let generated = response
            .metadata()
            .get(CORRELATION_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(uuid::Uuid::parse_str(generated).is_ok());
        assert_ne!(generated, context.id().to_string());
        let mut bad = Request::new(ListDirectoryRequest { path: "/".into() });
        bad.metadata_mut()
            .insert(CORRELATION_HEADER, "not-a-uuid".parse().unwrap());
        assert_eq!(
            client.list_directory(bad).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let mut duplicate = context.request(ListDirectoryRequest { path: "/".into() });
        duplicate.metadata_mut().append(
            CORRELATION_HEADER,
            context.id().to_string().parse().unwrap(),
        );
        assert_eq!(
            client.list_directory(duplicate).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    });
}

#[test]
fn store_reuses_context_for_probe_and_read() {
    let server = ObservedServer::new();
    let store = Store::new(
        &server.url,
        FsNode {
            node_id: "node".into(),
        },
    )
    .unwrap();
    let context = OperationContext::new();
    assert_eq!(
        store.read_in(&context, "/hello.txt", 0, 5).unwrap(),
        b"hello"
    );
    let ids = server.ids();
    assert!(
        ids.len() >= 2,
        "expected reachability probe and read: {ids:?}"
    );
    assert!(
        ids.iter().all(|id| id == &context.id().to_string()),
        "all calls must retain the original ID: {ids:?}"
    );
}

#[test]
fn concurrent_store_operations_keep_separate_ids() {
    let server = ObservedServer::new();
    let store = Arc::new(
        Store::new(
            &server.url,
            FsNode {
                node_id: "node".into(),
            },
        )
        .unwrap(),
    );
    let left = OperationContext::new();
    let right = OperationContext::new();
    let expected = [left.id().to_string(), right.id().to_string()];
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let handles: Vec<_> = [left, right]
        .into_iter()
        .map(|ctx| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                assert_eq!(store.read_in(&ctx, "/hello.txt", 0, 5).unwrap(), b"hello");
            })
        })
        .collect();
    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }
    let ids = server.ids();
    assert!(
        expected.iter().all(|id| ids.contains(id)),
        "each operation must reach the server with its own ID: {ids:?}"
    );
    assert!(
        ids.iter().all(|id| expected.contains(id)),
        "no request may invent or leak a different ID: {ids:?}"
    );
}

#[test]
fn fuse_listing_fanout_reuses_one_id_for_both_nodes() {
    if !super::common::require_fuse() {
        return;
    }
    let a = ObservedServer::new();
    let b = ObservedServer::new();
    std::fs::rename(
        b.root.path().join("hello.txt"),
        b.root.path().join("other.txt"),
    )
    .unwrap();
    let mount = tempfile::tempdir().unwrap();
    let nodes = vec![
        ("a".to_owned(), a.url.clone()),
        ("b".to_owned(), b.url.clone()),
    ];
    let session = punkfs::PunkFs::mount_cluster(
        &nodes,
        mount.path(),
        FsNode {
            node_id: "client".into(),
        },
    )
    .unwrap();
    let _ = std::fs::read_dir(mount.path()).unwrap().collect::<Vec<_>>();
    let ids_a = a.ids();
    let ids_b = b.ids();
    assert!(
        !ids_a.is_empty() && !ids_b.is_empty(),
        "both nodes must receive listing calls: {ids_a:?} {ids_b:?}"
    );
    assert!(
        ids_a.iter().any(|id| ids_b.contains(id)),
        "one FUSE operation must use the same ID on both nodes: {ids_a:?} {ids_b:?}"
    );
    assert!(
        ids_a
            .iter()
            .chain(&ids_b)
            .all(|id| uuid::Uuid::parse_str(id).is_ok())
    );
    let _ = std::fs::read_dir(mount.path()).unwrap().collect::<Vec<_>>();
    let after_a = a.ids();
    let after_b = b.ids();
    let next_a = &after_a[ids_a.len()..];
    let next_b = &after_b[ids_b.len()..];
    assert!(!next_a.is_empty() && !next_b.is_empty());
    assert!(next_a.iter().all(|id| !ids_a.contains(id)));
    assert!(next_b.iter().all(|id| !ids_b.contains(id)));
    assert!(next_a.iter().any(|id| next_b.contains(id)));
    drop(session);
}
