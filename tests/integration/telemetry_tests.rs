//! Correlation tests use synthetic interceptors, not application log events.
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use punkfs::grpc::service::PunkFsService;
use punkfs::punk_fs::punk_fs_server::PunkFsServer;
use punkfs::telemetry::{
    self, CORRELATION_HEADER, Config, Destination, OperationContext, Severity,
};
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
fn metadata_cache_correlation_child() {
    let Some(log_path) = std::env::var_os("PUNKFS_CACHE_TEST_LOG") else {
        return;
    };
    let path = std::path::PathBuf::from(log_path);
    let mut guard = telemetry::init_global(Config {
        threshold: Severity::Trace,
        destination: Destination::File(path.clone()),
        queue_capacity: 1024,
        max_file_bytes: 1_000_000,
        retained_files: 1,
        service: "test".into(),
    })
    .unwrap();
    let server = ObservedServer::new();
    let store = Store::new(
        &server.url,
        FsNode {
            node_id: "node".into(),
        },
    )
    .unwrap();
    let listing = OperationContext::new();
    let refresh = OperationContext::new();
    let rename = OperationContext::new();
    let unlink = OperationContext::new();
    store.ensure_listed_in(&listing, "/").unwrap();
    store.refresh_in(&refresh, "/hello.txt").unwrap();
    store
        .rename_in(
            &rename,
            "/hello.txt",
            "/renamed.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
    store.unlink_in(&unlink, "/renamed.txt").unwrap();
    assert!(telemetry::global().unwrap().flush(Duration::from_secs(2)));
    let read_events = || -> Vec<serde_json::Value> {
        std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let events = read_events();
    for (operation, ctx) in [
        ("reconcile", &listing),
        ("upsert", &refresh),
        ("rename", &rename),
        ("remove", &unlink),
    ] {
        assert!(
            events.iter().any(|e| e["component"] == "metadata_cache"
                && e["operation"] == operation
                && e["correlation_id"] == ctx.id().to_string()),
            "missing {operation} event for {}: {events:?}",
            ctx.id()
        );
        assert!(
            server.ids().contains(&ctx.id().to_string()),
            "cache event must share the outbound RPC ID"
        );
    }
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("hello.txt")
    );

    if super::common::require_fuse() {
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
        assert!(telemetry::global().unwrap().flush(Duration::from_secs(2)));
        let events = read_events();
        let ids_a = a.ids();
        let ids_b = b.ids();
        assert!(
            events
                .iter()
                .filter(|e| e["component"] == "metadata_cache" && e["operation"] == "reconcile")
                .any(|event| {
                    event["correlation_id"].as_str().is_some_and(|id| {
                        ids_a.iter().any(|a| a == id) && ids_b.iter().any(|b| b == id)
                    })
                }),
            "FUSE fan-out and cache event must share the operation ID"
        );
        drop(session);
    }
    assert!(guard.shutdown(Duration::from_secs(2)));
}

#[test]
fn metadata_cache_logs_share_store_and_fuse_rpc_correlation() {
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "telemetry_tests::metadata_cache_correlation_child",
            "--nocapture",
        ])
        .env("PUNKFS_CACHE_TEST_LOG", dir.path().join("events.jsonl"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn local_cache_telemetry_child() {
    let Some(log_path) = std::env::var_os("PUNKFS_LOCAL_TEST_LOG") else {
        return;
    };
    if !super::common::require_fuse() {
        return;
    }
    let log_path = std::path::PathBuf::from(log_path);
    let mut guard = telemetry::init_global(Config {
        threshold: Severity::Trace,
        destination: Destination::File(log_path.clone()),
        queue_capacity: 1024,
        max_file_bytes: 1_000_000,
        retained_files: 1,
        service: "test".into(),
    })
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = punkfs::PunkFs::mount_with_local(
        &[],
        root.path(),
        FsNode {
            node_id: "local".into(),
        },
    )
    .unwrap();
    std::fs::write(root.path().join("private-name"), b"private-contents").unwrap();
    assert!(telemetry::global().unwrap().flush(Duration::from_secs(2)));
    let raw = std::fs::read_to_string(&log_path).unwrap();
    assert!(!raw.contains("private-name") && !raw.contains("private-contents"));
    let events: Vec<serde_json::Value> = raw
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let create = events
        .iter()
        .find(|event| {
            event["component"] == "filesystem.passthrough"
                && event["operation"] == "create"
                && event["fields"]["outcome"] == "ok"
        })
        .expect("local create event");
    let id = create["correlation_id"].as_str().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event["component"] == "metadata_cache"
                && event["operation"] == "upsert"
                && event["correlation_id"] == id),
        "cache update must share FUSE create correlation ID"
    );
    drop(session);
    assert!(guard.shutdown(Duration::from_secs(2)));
}

#[test]
fn local_create_and_cache_events_share_correlation() {
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "telemetry_tests::local_cache_telemetry_child",
            "--nocapture",
        ])
        .env("PUNKFS_LOCAL_TEST_LOG", dir.path().join("events.jsonl"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn route_console_child() {
    let Some(path) = std::env::var_os("PUNKFS_ROUTE_TEST_LOG") else {
        return;
    };
    if !super::common::require_fuse() {
        return;
    }
    let mut guard = telemetry::init_global(Config {
        threshold: Severity::Info,
        destination: Destination::File(path.into()),
        queue_capacity: 1024,
        max_file_bytes: 1_000_000,
        retained_files: 1,
        service: "route-test".into(),
    })
    .unwrap();
    let server = super::common::TestServer::empty();
    server.seed("remote.txt", b"remote-content-secret");
    let mount = tempfile::tempdir().unwrap();
    std::fs::write(mount.path().join("local.txt"), b"local-content-secret").unwrap();
    let session = punkfs::PunkFs::mount_with_local_console(
        &[("remote-node".into(), server.url.clone())],
        mount.path(),
        FsNode {
            node_id: "local-node".into(),
        },
    )
    .unwrap();
    let _ = std::fs::read(mount.path().join("local.txt")).unwrap();
    let _ = std::fs::read(mount.path().join("remote.txt")).unwrap();
    let _ = std::fs::read_dir(mount.path()).unwrap().collect::<Vec<_>>();
    drop(session);
    assert!(guard.shutdown(Duration::from_secs(2)));
}

#[test]
fn route_console_identifies_local_remote_and_merged_entries() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("routes.jsonl");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "telemetry_tests::route_console_child",
            "--nocapture",
        ])
        .env("PUNKFS_ROUTE_TEST_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    if !super::common::require_fuse() {
        return;
    }
    let console = String::from_utf8_lossy(&output.stderr);
    assert!(
        console.contains("/local.txt") && console.contains("node=\"local-node\" location=local")
    );
    assert!(
        console.contains("/remote.txt") && console.contains("node=\"remote-node\" location=remote")
    );
    assert!(console.contains("location=merged"));
    assert!(!console.contains("content-secret"));
    let raw = std::fs::read_to_string(log).unwrap();
    let events: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let routes: Vec<_> = events
        .iter()
        .filter(|e| e["component"] == "filesystem.route")
        .collect();
    assert!(!routes.is_empty());
    assert!(routes.iter().all(|e| e["severity"] == "info"
        && e["console"] == true
        && e["correlation_id"].as_str().is_some()));
    assert!(routes.iter().any(|e| e["fields"]["path"] == "/local.txt"
        && e["fields"]["node_id"] == "local-node"
        && e["fields"]["location"] == "local"));
    assert!(routes.iter().any(|e| e["fields"]["path"] == "/remote.txt"
        && e["fields"]["node_id"] == "remote-node"
        && e["fields"]["location"] == "remote"));
    assert!(!raw.contains("content-secret"));
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
