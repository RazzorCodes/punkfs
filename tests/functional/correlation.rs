//! Correlation-propagation tests: the request identity must survive the full
//! trip through the wire. An in-process tonic server with a synthetic
//! interceptor stands in for the real network boundary, so these are whole-
//! operation (ingress -> rpc -> response) checks with a DI mock at the
//! system edge. No FUSE, no external process.

use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use punkfs::grpc::service::PunkFsService;
use punkfs::punk_fs::ListDirectoryRequest;
use punkfs::punk_fs::punk_fs_client::PunkFsClient;
use punkfs::punk_fs::punk_fs_server::PunkFsServer;
use punkfs::telemetry::{CORRELATION_HEADER, OperationContext};
use punkfs::{FsNode, Store, StoreApi};
use tonic::Request;

/// A live punkfs grpc server with an interceptor that records every
/// correlation id it saw, so a test can assert what reached the wire.
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
        std::os::unix::fs::symlink("hello.txt", root.path().join("link")).unwrap();
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
                let interceptor = move |req: Request<()>| -> Result<Request<()>, tonic::Status> {
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

/// Ingress validates and preserves the correlation id: a caller-supplied id
/// is echoed back, an absent id is minted, and malformed/duplicate ids are
/// rejected.
#[test]
fn functional_grpc_ingress_validates_and_preserves_correlation() {
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

/// One store call (reachability probe + read) reuses the operation's id.
#[test]
fn functional_store_reuses_context_for_probe_and_read() {
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
    assert!(ids.len() >= 2, "expected probe and read: {ids:?}");
    assert!(
        ids.iter().all(|id| id == &context.id().to_string()),
        "all calls must retain the original ID: {ids:?}"
    );
}

/// A remote readlink carries its operation context over the wire unchanged.
#[test]
fn functional_readlink_preserves_correlation() {
    let server = ObservedServer::new();
    let store = Store::new(
        &server.url,
        FsNode {
            node_id: "node".into(),
        },
    )
    .unwrap();
    let context = OperationContext::new();
    assert_eq!(store.readlink_in(&context, "/link").unwrap(), b"hello.txt");
    let ids = server.ids();
    assert!(!ids.is_empty());
    assert!(ids.iter().all(|id| id == &context.id().to_string()));
}

/// Concurrent store operations keep their ids separate — no leak, no reuse.
#[test]
fn functional_concurrent_store_operations_keep_separate_ids() {
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
