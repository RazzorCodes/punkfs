//! Startup inventory exchange and best-effort publication of new files and symlinks.
//!
//! Inventories only add known contributions; they are never deletion scans.
//! Connections are statically configured and are not authenticated.
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use tokio::sync::mpsc;
use tonic::transport::Endpoint;
use tonic::{Request, Response, Status};

use crate::core::backing::Backing;
use crate::grpc::service::to_proto_attr;
use crate::punk_fs::peer_metadata_client::PeerMetadataClient;
use crate::punk_fs::peer_metadata_server::PeerMetadata;
use crate::punk_fs::{
    FileKind, HandshakeRequest, HandshakeResponse, PublishCreateRequest, PublishCreateResponse,
    PublishedEntry,
};
use crate::store::{Store, entry_from_attr};
use crate::telemetry::{self, Event, OperationContext, Severity};

const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const QUEUE_CAPACITY: usize = 64;
const MAX_CREATE_BYTES: usize = 64 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(2);

fn log(
    ctx: &OperationContext,
    severity: Severity,
    operation: &str,
    message: &str,
    node: &str,
    peer: Option<&str>,
    count: Option<usize>,
) {
    let Some(logger) = telemetry::global() else {
        return;
    };
    if !logger.enabled(severity) {
        return;
    }
    let mut event = Event::new(severity, "peer.metadata", message);
    event.operation = Some(operation.into());
    event
        .fields
        .insert("node_id".into(), serde_json::json!(node));
    if let Some(peer) = peer {
        event
            .fields
            .insert("peer_id".into(), serde_json::json!(peer));
    }
    if let Some(count) = count {
        event
            .fields
            .insert("entries".into(), serde_json::json!(count));
    }
    logger.emit_in(ctx, event);
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 4096
        && path != "/"
        && !path.as_bytes().contains(&0)
        && path[1..]
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn validate(entries: &[PublishedEntry], create: bool) -> Result<(), Status> {
    if entries.len() > MAX_ENTRIES
        || entries.iter().map(Message::encoded_len).sum::<usize>()
            > if create { MAX_CREATE_BYTES } else { MAX_BYTES }
    {
        return Err(Status::resource_exhausted("metadata batch exceeds limit"));
    }
    let mut names = HashMap::new();
    let mut files = 0;
    for entry in entries {
        if !valid_path(&entry.path) || names.contains_key(entry.path.as_str()) {
            return Err(Status::invalid_argument(
                "invalid or duplicate metadata path",
            ));
        }
        let attr = entry
            .attr
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing metadata"))?;
        if uuid::Uuid::parse_str(&attr.guid).is_err() {
            return Err(Status::invalid_argument("invalid guid"));
        }
        match FileKind::try_from(attr.kind) {
            Ok(FileKind::Directory) => {
                names.insert(entry.path.as_str(), true);
            }
            Ok(FileKind::File | FileKind::Symlink) => {
                files += 1;
                names.insert(entry.path.as_str(), false);
            }
            _ => return Err(Status::invalid_argument("unsupported metadata kind")),
        }
    }
    if create && files != 1 {
        return Err(Status::invalid_argument(
            "create publication must contain one file or symlink",
        ));
    }
    for entry in entries {
        let mut parent = Path::new(&entry.path).parent();
        while let Some(path) = parent {
            if path == Path::new("/") || path.as_os_str().is_empty() {
                break;
            }
            let exists = path.to_str().and_then(|parent| names.get(parent)).copied() == Some(true);
            if !exists {
                return Err(Status::invalid_argument("missing directory metadata"));
            }
            parent = path.parent();
        }
    }
    if create {
        let file = entries
            .iter()
            .find(|entry| {
                entry.attr.as_ref().is_some_and(|attr| {
                    attr.kind == FileKind::File as i32 || attr.kind == FileKind::Symlink as i32
                })
            })
            .expect("one file or symlink");
        if entries
            .iter()
            .any(|entry| entry.path != file.path && !Path::new(&file.path).starts_with(&entry.path))
        {
            return Err(Status::invalid_argument("unrelated create metadata"));
        }
    }
    Ok(())
}

fn apply(
    store: &Store,
    ctx: &OperationContext,
    peer: &str,
    mut entries: Vec<PublishedEntry>,
) -> usize {
    entries.sort_by_key(|entry| entry.path.len());
    let entries = entries
        .into_iter()
        .map(|entry| entry_from_attr(0, &entry.path, entry.attr.expect("validated"), peer))
        .collect();
    store.ingest_peer_entries(ctx, peer, entries)
}

fn published(backing: &Backing, path: &str) -> io::Result<PublishedEntry> {
    let real = backing.entry_path(path)?;
    let attr = backing.attr_for(path, &real)?;
    Ok(PublishedEntry {
        path: path.into(),
        attr: Some(to_proto_attr(&attr)),
    })
}

/// Scan only the local backing descriptor. Failure means no complete inventory;
/// callers must not translate it into an empty snapshot or a deletion.
fn inventory(backing: &Backing, _ctx: &OperationContext) -> io::Result<Vec<PublishedEntry>> {
    let mut queue = VecDeque::from(["/".to_owned()]);
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    while let Some(directory) = queue.pop_front() {
        let real = backing.existing(&directory)?;
        for item in std::fs::read_dir(&real)? {
            let item = item?;
            let name = item
                .file_name()
                .into_string()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 backing name"))?;
            let path = if directory == "/" {
                format!("/{name}")
            } else {
                format!("{directory}/{name}")
            };
            if !valid_path(&path) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid backing name",
                ));
            }
            let entry = published(backing, &path)?;
            if entry
                .attr
                .as_ref()
                .is_some_and(|a| a.kind == FileKind::Directory as i32)
            {
                queue.push_back(path);
            }
            bytes = bytes.saturating_add(entry.encoded_len());
            entries.push(entry);
            if entries.len() > MAX_ENTRIES || bytes > MAX_BYTES {
                return Err(io::Error::other("backing inventory exceeds metadata limit"));
            }
        }
    }
    Ok(entries)
}

pub(crate) struct Publication {
    ctx: OperationContext,
    entries: Vec<PublishedEntry>,
}

/// Lightweight, nonblocking producer used after a successful local create.
#[derive(Clone)]
pub(crate) struct Publisher {
    backing: Arc<Backing>,
    tx: mpsc::Sender<Publication>,
}

impl Publisher {
    pub(crate) fn created(&self, ctx: &OperationContext, path: &str) {
        let result = (|| -> io::Result<Vec<PublishedEntry>> {
            let mut paths = Vec::new();
            let mut current = Path::new(path).parent();
            while let Some(parent) = current {
                let p = parent
                    .to_str()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 parent"))?;
                if p.is_empty() || p == "/" {
                    break;
                }
                paths.push(p.to_owned());
                current = parent.parent();
            }
            paths.reverse();
            paths.push(path.to_owned());
            paths
                .iter()
                .map(|path| published(&self.backing, path))
                .collect()
        })();
        match result {
            Ok(entries) => {
                if validate(&entries, true).is_err() {
                    log(
                        ctx,
                        Severity::Warning,
                        "publish_create",
                        "creation metadata exceeds publication limits",
                        self.backing.node_id(),
                        None,
                        None,
                    );
                    return;
                }
                if self
                    .tx
                    .try_send(Publication {
                        ctx: ctx.clone(),
                        entries,
                    })
                    .is_err()
                {
                    log(
                        ctx,
                        Severity::Warning,
                        "publish_create",
                        "metadata publication queue unavailable",
                        self.backing.node_id(),
                        None,
                        None,
                    );
                }
            }
            Err(_) => log(
                ctx,
                Severity::Warning,
                "publish_create",
                "could not collect creation metadata",
                self.backing.node_id(),
                None,
                None,
            ),
        }
    }
}

pub(crate) fn publisher(backing: Arc<Backing>) -> (Publisher, mpsc::Receiver<Publication>) {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    (Publisher { backing, tx }, rx)
}

pub(crate) struct Control {
    backing: Arc<Backing>,
    store: Arc<Store>,
    peers: HashSet<String>,
}
impl Control {
    pub(crate) fn new(
        backing: Arc<Backing>,
        store: Arc<Store>,
        peers: &[(String, String)],
    ) -> Self {
        Self {
            backing,
            store,
            peers: peers.iter().map(|(id, _)| id.clone()).collect(),
        }
    }
    fn check(&self, peer: &str) -> Result<(), Status> {
        if !self.peers.contains(peer) {
            return Err(Status::permission_denied("peer is not configured"));
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl PeerMetadata for Control {
    async fn handshake(
        &self,
        request: Request<HandshakeRequest>,
    ) -> Result<Response<HandshakeResponse>, Status> {
        let ctx = OperationContext::from_request(&request)?;
        let req = request.into_inner();
        if let Err(err) = self
            .check(&req.node_id)
            .and_then(|_| validate(&req.entries, false))
        {
            log(
                &ctx,
                Severity::Warning,
                "handshake",
                "peer handshake rejected",
                self.backing.node_id(),
                None,
                None,
            );
            return Err(err);
        }
        let backing = self.backing.clone();
        let scan_context = ctx.clone();
        let snapshot = tokio::task::spawn_blocking(move || inventory(&backing, &scan_context))
            .await
            .map_err(|_| Status::internal("inventory worker failed"))?
            .map_err(|_| Status::unavailable("local inventory unavailable"));
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(err) => {
                log(
                    &ctx,
                    Severity::Warning,
                    "handshake",
                    "local inventory unavailable",
                    self.backing.node_id(),
                    Some(&req.node_id),
                    None,
                );
                return Err(err);
            }
        };
        let count = apply(&self.store, &ctx, &req.node_id, req.entries);
        log(
            &ctx,
            Severity::Info,
            "handshake",
            "peer inventory accepted",
            self.backing.node_id(),
            Some(&req.node_id),
            Some(count),
        );
        Ok(ctx.response(HandshakeResponse {
            node_id: self.backing.node_id().into(),
            entries: snapshot,
        }))
    }
    async fn publish_create(
        &self,
        request: Request<PublishCreateRequest>,
    ) -> Result<Response<PublishCreateResponse>, Status> {
        let ctx = OperationContext::from_request(&request)?;
        let req = request.into_inner();
        if let Err(err) = self
            .check(&req.node_id)
            .and_then(|_| validate(&req.entries, true))
        {
            log(
                &ctx,
                Severity::Warning,
                "publish_create",
                "creation metadata rejected",
                self.backing.node_id(),
                None,
                None,
            );
            return Err(err);
        }
        let count = apply(&self.store, &ctx, &req.node_id, req.entries);
        log(
            &ctx,
            Severity::Debug,
            "publish_create",
            "creation metadata accepted",
            self.backing.node_id(),
            Some(&req.node_id),
            Some(count),
        );
        Ok(ctx.response(PublishCreateResponse {}))
    }
}

fn client(
    url: &str,
) -> Result<PeerMetadataClient<tonic::transport::Channel>, tonic::transport::Error> {
    let channel = Endpoint::from_shared(url.to_owned())?
        .connect_timeout(RPC_TIMEOUT)
        .timeout(RPC_TIMEOUT)
        .connect_lazy();
    Ok(PeerMetadataClient::new(channel))
}

/// Start one retrying initial handshake per peer and the creation publisher.
pub(crate) fn start(
    runtime: &tokio::runtime::Runtime,
    backing: Arc<Backing>,
    store: Arc<Store>,
    peers: Vec<(String, String)>,
    mut rx: mpsc::Receiver<Publication>,
) {
    for (peer, url) in peers.clone() {
        let backing = backing.clone();
        let store = store.clone();
        runtime.spawn(async move {
            let mut delay = Duration::from_millis(250);
            let mut first = true;
            let ctx = OperationContext::new();
            loop {
                let scan = backing.clone();
                let scan_context = ctx.clone();
                let result = async {
                    let endpoint = Endpoint::from_shared(url.clone())
                        .map_err(|_| Status::invalid_argument("invalid endpoint"))?
                        .connect_timeout(RPC_TIMEOUT)
                        .timeout(RPC_TIMEOUT);
                    let channel = endpoint
                        .connect()
                        .await
                        .map_err(|_| Status::unavailable("peer unavailable"))?;
                    let mut client = PeerMetadataClient::new(channel);
                    let entries =
                        tokio::task::spawn_blocking(move || inventory(&scan, &scan_context))
                            .await
                            .map_err(|_| Status::internal("inventory worker failed"))?
                            .map_err(|_| Status::unavailable("inventory unavailable"))?;
                    let response = client
                        .handshake(ctx.request(HandshakeRequest {
                            node_id: backing.node_id().into(),
                            entries,
                        }))
                        .await?
                        .into_inner();
                    if response.node_id != peer {
                        return Err(Status::failed_precondition("peer identity mismatch"));
                    }
                    validate(&response.entries, false)?;
                    Ok::<usize, Status>(apply(&store, &ctx, &peer, response.entries))
                }
                .await;
                match result {
                    Ok(count) => {
                        log(
                            &ctx,
                            Severity::Info,
                            "handshake",
                            "peer handshake completed",
                            backing.node_id(),
                            Some(&peer),
                            Some(count),
                        );
                        break;
                    }
                    Err(_) => {
                        log(
                            &ctx,
                            if first {
                                Severity::Warning
                            } else {
                                Severity::Debug
                            },
                            "handshake",
                            "peer handshake failed; retrying",
                            backing.node_id(),
                            Some(&peer),
                            None,
                        );
                        first = false;
                    }
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        });
    }
    runtime.spawn(async move {
        while let Some(publication) = rx.recv().await {
            let mut tasks = tokio::task::JoinSet::new();
            for (peer, url) in peers.clone() {
                let ctx = publication.ctx.clone();
                let entries = publication.entries.clone();
                let node = backing.node_id().to_owned();
                tasks.spawn(async move {
                    let mut result = Err(Status::unavailable("publication not attempted"));
                    for attempt in 0..3 {
                        result = async {
                            validate(&entries, true)?;
                            let mut client = client(&url)
                                .map_err(|_| Status::unavailable("invalid endpoint"))?;
                            client
                                .publish_create(ctx.request(PublishCreateRequest {
                                    node_id: node.clone(),
                                    entries: entries.clone(),
                                }))
                                .await?;
                            Ok::<(), Status>(())
                        }
                        .await;
                        let retry = result.as_ref().err().is_some_and(|err| {
                            matches!(
                                err.code(),
                                tonic::Code::Unavailable | tonic::Code::DeadlineExceeded
                            )
                        });
                        if !retry || attempt == 2 {
                            break;
                        }
                        log(
                            &ctx,
                            Severity::Debug,
                            "publish_create",
                            "retrying creation metadata",
                            &node,
                            Some(&peer),
                            None,
                        );
                        tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
                    }
                    log(
                        &ctx,
                        if result.is_ok() {
                            Severity::Debug
                        } else {
                            Severity::Warning
                        },
                        "publish_create",
                        if result.is_ok() {
                            "creation metadata published"
                        } else {
                            "creation metadata publication failed"
                        },
                        &node,
                        Some(&peer),
                        None,
                    );
                });
            }
            while tasks.join_next().await.is_some() {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsNode, StoreApi};

    fn file(path: &str) -> PublishedEntry {
        PublishedEntry {
            path: path.into(),
            attr: Some(crate::punk_fs::FileAttr {
                guid: uuid::Uuid::new_v4().to_string(),
                kind: FileKind::File as i32,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn metadata_validation_rejects_bad_batches_before_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let backing = Arc::new(Backing::open(dir.path(), "self").unwrap());
        let peers = vec![("peer".to_owned(), "http://127.0.0.1:1".to_owned())];
        let store = Arc::new(
            Store::new_with_local(
                &peers,
                FsNode {
                    node_id: "self".into(),
                },
            )
            .unwrap(),
        );
        let control = Control::new(backing, store.clone(), &peers);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let bad_peer = control
                .publish_create(Request::new(PublishCreateRequest {
                    node_id: "other".into(),
                    entries: vec![file("/good")],
                }))
                .await
                .unwrap_err();
            assert_eq!(bad_peer.code(), tonic::Code::PermissionDenied);
            let bad_path = control
                .handshake(Request::new(HandshakeRequest {
                    node_id: "peer".into(),
                    entries: vec![file("/good"), file("/../escape")],
                }))
                .await
                .unwrap_err();
            assert_eq!(bad_path.code(), tonic::Code::InvalidArgument);
            assert!(store.get_inode("/good").is_none());
            let missing_parent = control
                .publish_create(Request::new(PublishCreateRequest {
                    node_id: "peer".into(),
                    entries: vec![file("/missing/file")],
                }))
                .await
                .unwrap_err();
            assert_eq!(missing_parent.code(), tonic::Code::InvalidArgument);
        });
    }

    #[test]
    fn announcements_reserve_conflicts_without_overwriting_the_owner() {
        let peers = vec![
            ("a".into(), "http://127.0.0.1:1".into()),
            ("b".into(), "http://127.0.0.1:2".into()),
        ];
        let store = Store::new_with_local(
            &peers,
            FsNode {
                node_id: "self".into(),
            },
        )
        .unwrap();
        let ctx = OperationContext::new();
        let a = file("/same");
        assert_eq!(apply(&store, &ctx, "a", vec![a.clone()]), 1);
        assert_eq!(apply(&store, &ctx, "a", vec![a]), 0);
        assert_eq!(apply(&store, &ctx, "b", vec![file("/same")]), 0);
        assert!(store.has_conflict("/same"));
        assert_eq!(store.owner_of("/same").as_deref(), Some("a"));
    }
}
