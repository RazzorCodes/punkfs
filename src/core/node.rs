//! Lifecycle for a node that mounts and serves one backing directory.
use std::collections::HashSet;
use std::error::Error;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

use crate::core::backing::Backing;
use crate::core::cache::metadata::FsNode;
use crate::core::punkfs::Fs;
use crate::filesystem::passthrough::Local;
use crate::grpc::peer;
use crate::grpc::service::PunkFsService;
use crate::punk_fs::peer_metadata_server::PeerMetadataServer;
use crate::punk_fs::punk_fs_server::PunkFsServer;
use crate::store::Store;
use crate::telemetry::{self, Event, Severity};

/// Configuration for one mounted and served backing directory.
pub struct NodeConfig {
    pub mountpoint: PathBuf,
    pub node_id: String,
    pub listen: SocketAddr,
    pub peers: Vec<(String, String)>,
    pub route_console: bool,
    pub db_path: Option<PathBuf>,
    pub cache_ttl: Option<Duration>,
    pub cache_max_entries: Option<usize>,
}

fn lifecycle(severity: Severity, message: &str, node: &str) {
    if let Some(logger) = telemetry::global() {
        let mut event = Event::new(severity, "node.lifecycle", message);
        event
            .fields
            .insert("node_id".into(), serde_json::json!(node));
        logger.emit(event);
    }
}

/// Run until a termination signal or an unexpected local component exit.
/// This is synchronous so that FUSE and the existing synchronous store are
/// not created or dropped inside an async runtime.
pub fn run(config: NodeConfig) -> Result<(), Box<dyn Error>> {
    let node = config.node_id.clone();
    let result = run_inner(config);
    if result.is_err() {
        lifecycle(Severity::Error, "node failed", &node);
    }
    result
}

fn run_inner(config: NodeConfig) -> Result<(), Box<dyn Error>> {
    if config.node_id.is_empty() {
        return Err("node id must not be empty".into());
    }
    let mut ids = HashSet::new();
    for (id, url) in &config.peers {
        if id.is_empty() || url.is_empty() || id == &config.node_id || !ids.insert(id) {
            return Err("peer IDs must be nonempty, unique, and different from the local ID; URLs must not be empty".into());
        }
    }

    // Pin and bind before mounting: any failure leaves the directory uncovered.
    let backing = Arc::new(Backing::open(&config.mountpoint, &config.node_id)?);
    backing.reject_fuse_mount()?;
    let listener = TcpListener::bind(config.listen)?;
    listener.set_nonblocking(true)?;
    let actual_addr = listener.local_addr()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(32)
        .enable_all()
        .build()?;
    let local = Local::from_backing(backing.clone());
    let (publisher, publications) = peer::publisher(backing.clone());
    local.set_publisher(publisher);
    let store = Arc::new(Store::new_with_local_persistence(
        &config.peers,
        FsNode {
            node_id: config.node_id.clone(),
        },
        config.db_path.as_deref(),
        config.cache_ttl,
        config.cache_max_entries,
    )?);

    // Scan before exposing the FUSE mount: otherwise a lookup can race the
    // initial snapshot and block it while reconciling the whole backing tree.
    let scan_ctx = crate::telemetry::OperationContext::new();
    let scanned = peer::scan_local_startup(&backing, &store, &scan_ctx)?;
    if let Some(logger) = telemetry::global() {
        let mut event = Event::new(
            Severity::Info,
            "node.lifecycle",
            "startup local scan completed",
        );
        event
            .fields
            .insert("node_id".into(), serde_json::json!(&config.node_id));
        event
            .fields
            .insert("entries".into(), serde_json::json!(scanned));
        logger.emit_in(&scan_ctx, event);
    }
    let session = Fs::mount_with_store(
        &config.mountpoint,
        config.route_console,
        local,
        store.clone(),
    )?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let control = peer::Control::new(backing.clone(), store.clone(), &config.peers);
    peer::start(
        &runtime,
        backing.clone(),
        store.clone(),
        config.peers.clone(),
        publications,
    );
    let service = PunkFsService::from_backing_with_catalog(backing, false, Some(store.clone()));
    let mut server = runtime.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        Server::builder()
            .concurrency_limit_per_connection(32)
            .add_service(PunkFsServer::from_arc(service))
            .add_service(PeerMetadataServer::new(control))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await
            .map_err(std::io::Error::other)
    });
    lifecycle(Severity::Info, "node started", &config.node_id);
    println!(
        "mounted {} and serving {} as {}",
        config.mountpoint.display(),
        actual_addr,
        config.node_id
    );

    let mut server_observed = false;
    let reason: Result<(), Box<dyn Error>> = runtime.block_on(async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        loop {
            tokio::select! {
                result = tokio::signal::ctrl_c() => { result?; break Ok(()); }
                _ = term.recv() => break Ok(()),
                result = &mut server => {
                    server_observed = true;
                    break Err(format!("backing server stopped unexpectedly: {result:?}").into());
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if session.guard.is_finished() { break Err("FUSE session stopped unexpectedly".into()); }
                }
            }
        }
    });
    lifecycle(
        if reason.is_ok() {
            Severity::Info
        } else {
            Severity::Error
        },
        "node stopping",
        &config.node_id,
    );
    let _ = shutdown_tx.send(());
    // Keep the async runtime alive while the FUSE thread completes outstanding
    // requests and while the server drains. An unmount error remains visible.
    let (unmount_tx, unmount_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = unmount_tx.send(session.umount_and_join());
    });
    let unmount = unmount_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| std::io::Error::other("FUSE unmount did not finish within 5 seconds"))
        .and_then(|result| result);
    let server_result =
        if server_observed {
            None
        } else {
            Some(runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), &mut server).await
            }))
        };
    if let Some(result) = server_result {
        match result {
            Ok(Ok(Ok(()))) => {}
            other => {
                server.abort();
                lifecycle(
                    Severity::Warning,
                    "server did not shut down cleanly",
                    &config.node_id,
                );
                runtime.shutdown_timeout(Duration::from_secs(1));
                return Err(format!("server shutdown: {other:?}").into());
            }
        }
    }
    runtime.shutdown_timeout(Duration::from_secs(1));
    unmount?;
    reason
}
