//! The client-side store: the only way the filesystem layer reaches the
//! catalog.
//!
//! It owns the local snapshot of the server's tree ([`MetadataCache`]), the grpc
//! client, and the tokio runtime used to drive async calls from the
//! synchronous FUSE callbacks.
//!
//! Identity note: entries are identified by `guid` (the punkfs identity,
//! assigned by the server) and addressed locally by `inode` (allocated
//! here). The store is the layer that maps between the two.
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use fuser::RenameFlags;
use tokio::runtime::Runtime;
use tonic::transport::{Channel, Endpoint};

use self::db::attributes::{Access, FileAttrData, FileTimes, FsEntryKind, Stat};
use self::db::metadata_cache::{Availability, FsEntry, FsNode, ListingOutcome, MetadataCache};
use crate::punk_fs::punk_fs_client::PunkFsClient;
use crate::punk_fs::{
    self, FsyncFileRequest, GetAttrRequest, ListDirectoryRequest, ReadFileRequest, ReadLinkRequest,
    SetAttrRequest, StatFsRequest, WriteFileRequest,
};
use crate::telemetry::OperationContext;
pub(crate) mod contextual;
pub mod db;

#[cfg(test)]
pub(crate) mod mock;

/// Everything the filesystem layer can get back from a store call.
///
/// The op modules translate this into an `OpError` (and thus an errno);
/// the store itself never knows about FUSE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The path/inode is not known to the catalog.
    NotFound,
    /// The backend refuses the operation (policy, permissions, ...).
    NotPermitted,
    /// The operation is not supported by this backend at all.
    Unsupported(String),
    /// The node owning this entry did not answer (its identity is carried
    /// so a caller can name the culprit).
    Inaccessible(String),
    /// Anything from the transport or the server.
    Io(String),
    /// A local syscall failed with its original errno.
    Local(i32),
}

impl From<tonic::Status> for StoreError {
    fn from(status: tonic::Status) -> Self {
        if let Some(errno) = status
            .metadata()
            .get("x-punkfs-errno")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i32>().ok())
        {
            return StoreError::Local(errno);
        }
        match status.code() {
            tonic::Code::NotFound => StoreError::NotFound,
            tonic::Code::Unavailable => StoreError::Inaccessible("unavailable".into()),
            tonic::Code::PermissionDenied => StoreError::NotPermitted,
            tonic::Code::Unimplemented => StoreError::Unsupported(status.message().to_owned()),
            tonic::Code::InvalidArgument => StoreError::Unsupported(status.message().to_owned()),
            _ => StoreError::Io(status.to_string()),
        }
    }
}

/// One child of a directory, as `readdir` needs it: the inode to report,
/// the name to show, and the kind so `ls -F`/`find` don't lie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreChild {
    pub inode: u64,
    pub name: String,
    pub kind: FsEntryKind,
}

/// Requested attribute changes from one `setattr` call.
///
/// `None` means "leave this alone". Times use signed seconds and optional
/// nanoseconds; the operation layer resolves `TimeOrNow`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetattrChanges {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub accessed_unix: Option<i64>,
    pub modified_unix: Option<i64>,
    pub accessed_nanos: Option<u32>,
    pub modified_nanos: Option<u32>,
}

/// Filesystem-wide capacity counters backing `statfs` / `df`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatFs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub block_size: u32,
    pub name_length: u32,
    pub fragment_size: u32,
}

/// The filesystem's only view of the catalog.
///
/// The op modules (`crate::filesystem::*`) take this as a trait object so
/// every method can be unit tested against a mock store without a
/// server, a runtime, or a FUSE session.
pub trait StoreApi: Send + Sync {
    /// The ingress operation bound to this store view, if any.
    fn operation_context(&self) -> Option<&OperationContext> {
        None
    }

    /// This machine's identity, as registered with the server.
    fn node_id(&self) -> &str;

    // --- catalog -------------------------------------------------------
    /// Inode for `path`, if the catalog knows it.
    fn get_inode(&self, path: &str) -> Option<u64>;
    /// Path for `inode`, if the catalog knows it.
    fn get_path(&self, inode: u64) -> Option<String>;
    /// The whole entry (kind, metadata, path, guid) for an inode.
    fn get_entry(&self, inode: u64) -> Option<FsEntry>;
    /// Which backing node holds `path`, if the catalog knows it.
    fn owner_of(&self, path: &str) -> Option<String>;
    /// Fetch and reconcile `dir` from the server.
    fn ensure_listed(&self, dir: &str) -> Result<(), StoreError>;
    /// Children of `path`, using whatever listing is cached.
    fn get_children(&self, path: &str) -> Vec<StoreChild>;
    /// Read already-reconciled children without issuing a network request.
    fn cached_children(&self, path: &str) -> Vec<StoreChild> {
        self.get_children(path)
    }
    /// Whether a logical name has multiple known owners.
    fn has_conflict(&self, _path: &str) -> bool {
        false
    }
    /// The local configuration can mount with no remote backends.
    fn has_remote_nodes(&self) -> bool {
        true
    }

    // --- content -------------------------------------------------------
    /// Return a symlink's target bytes without following it.
    fn readlink(&self, _path: &str) -> Result<Vec<u8>, StoreError> {
        Err(StoreError::Unsupported("readlink".into()))
    }
    /// Read at most `size` bytes at `offset`; a short read means EOF.
    fn read(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>, StoreError>;
    /// Write `data` at `offset`; returns how many bytes were taken.
    fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<u32, StoreError>;
    /// Durability barrier for `path`; `datasync` skips metadata.
    fn fsync(&self, path: &str, datasync: bool) -> Result<(), StoreError>;

    // --- metadata ------------------------------------------------------
    fn get_xattr(&self, _path: &str, _name: &[u8]) -> Result<Vec<u8>, StoreError> {
        Err(StoreError::Unsupported("xattr".into()))
    }
    fn list_xattr(&self, _path: &str) -> Result<Vec<u8>, StoreError> {
        Err(StoreError::Unsupported("xattr".into()))
    }
    fn set_xattr(
        &self,
        _path: &str,
        _name: &[u8],
        _value: &[u8],
        _flags: i32,
    ) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("xattr".into()))
    }
    fn remove_xattr(&self, _path: &str, _name: &[u8]) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("xattr".into()))
    }
    fn get_xattr_in(
        &self,
        _ctx: &OperationContext,
        path: &str,
        name: &[u8],
    ) -> Result<Vec<u8>, StoreError> {
        self.get_xattr(path, name)
    }
    fn list_xattr_in(&self, _ctx: &OperationContext, path: &str) -> Result<Vec<u8>, StoreError> {
        self.list_xattr(path)
    }
    fn set_xattr_in(
        &self,
        _ctx: &OperationContext,
        path: &str,
        name: &[u8],
        value: &[u8],
        flags: i32,
    ) -> Result<(), StoreError> {
        self.set_xattr(path, name, value, flags)
    }
    fn remove_xattr_in(
        &self,
        _ctx: &OperationContext,
        path: &str,
        name: &[u8],
    ) -> Result<(), StoreError> {
        self.remove_xattr(path, name)
    }
    /// Apply the requested attribute changes, returning the new attributes.
    fn setattr(&self, path: &str, changes: SetattrChanges) -> Result<FileAttrData, StoreError>;
    /// Re-read one entry's attributes, bypassing the cache.
    fn refresh(&self, path: &str) -> Result<FileAttrData, StoreError>;
    /// Aggregate capacity for `statfs`.
    fn statfs(&self) -> Result<StatFs, StoreError>;

    // --- namespace mutation --------------------------------------------
    // Local creation is handled by the passthrough layer; the remote API
    // has no creation RPC.
    /// Remove the file at `path`.
    fn unlink(&self, path: &str) -> Result<(), StoreError>;
    /// Remove the *empty* directory at `path`.
    fn rmdir(&self, path: &str) -> Result<(), StoreError>;
    /// Move `old` to `new`, honouring `flags`.
    fn rename(&self, old: &str, new: &str, flags: RenameFlags) -> Result<(), StoreError>;

    // Context-aware entry points. Existing mock/embedded implementations can
    // retain their original behavior; the network store overrides these.
    fn ensure_listed_in(&self, _ctx: &OperationContext, dir: &str) -> Result<(), StoreError> {
        self.ensure_listed(dir)
    }
    fn get_children_in(&self, _ctx: &OperationContext, path: &str) -> Vec<StoreChild> {
        self.get_children(path)
    }
    fn readlink_in(&self, _ctx: &OperationContext, path: &str) -> Result<Vec<u8>, StoreError> {
        self.readlink(path)
    }
    fn read_in(
        &self,
        _ctx: &OperationContext,
        path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, StoreError> {
        self.read(path, offset, size)
    }
    fn write_in(
        &self,
        _ctx: &OperationContext,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<u32, StoreError> {
        self.write(path, offset, data)
    }
    fn fsync_in(
        &self,
        _ctx: &OperationContext,
        path: &str,
        datasync: bool,
    ) -> Result<(), StoreError> {
        self.fsync(path, datasync)
    }
    fn setattr_in(
        &self,
        _ctx: &OperationContext,
        path: &str,
        changes: SetattrChanges,
    ) -> Result<FileAttrData, StoreError> {
        self.setattr(path, changes)
    }
    fn refresh_in(&self, _ctx: &OperationContext, path: &str) -> Result<FileAttrData, StoreError> {
        self.refresh(path)
    }
    fn statfs_in(&self, _ctx: &OperationContext) -> Result<StatFs, StoreError> {
        self.statfs()
    }
    fn unlink_in(&self, _ctx: &OperationContext, path: &str) -> Result<(), StoreError> {
        self.unlink(path)
    }
    fn rmdir_in(&self, _ctx: &OperationContext, path: &str) -> Result<(), StoreError> {
        self.rmdir(path)
    }
    fn rename_in(
        &self,
        _ctx: &OperationContext,
        old: &str,
        new: &str,
        flags: RenameFlags,
    ) -> Result<(), StoreError> {
        self.rename(old, new, flags)
    }

    /// Reconcile local directory entries without contacting a server.
    fn reconcile_local_in(
        &self,
        _ctx: &OperationContext,
        _dir: &str,
        _entries: Vec<FsEntry>,
    ) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("local catalog unavailable".into()))
    }
    /// Cache one newly created or refreshed local entry, returning its inode.
    fn upsert_local_in(&self, _ctx: &OperationContext, _entry: FsEntry) -> Result<u64, StoreError> {
        Err(StoreError::Unsupported("local catalog unavailable".into()))
    }
    fn remove_local_in(&self, _ctx: &OperationContext, _path: &str) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("local catalog unavailable".into()))
    }
    fn exchange_local_in(
        &self,
        _ctx: &OperationContext,
        _old: &str,
        _new: &str,
    ) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("local catalog unavailable".into()))
    }
    fn rename_local_in(
        &self,
        _ctx: &OperationContext,
        _old: &str,
        _new: &str,
    ) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("local catalog unavailable".into()))
    }
}

pub struct Store {
    metadata_cache: Mutex<MetadataCache>,
    // Prevent an older in-flight listing from erasing a newer peer notice.
    peer_generation: Mutex<HashMap<String, u64>>,
    listed: Mutex<HashSet<String>>, // directories already fetched from the server
    rt: Runtime,
    /// One connection per backing node, in stable merge order.
    nodes: Vec<NodeConn>,
    node: FsNode, // where am i running right now
    /// Paths contributed by more than one node in the last listing.
    conflicts: Mutex<HashSet<String>>,
    local_node_enabled: bool,
}

/// A reachable backing node: its identity, its live connection, and when
/// it last answered.
struct NodeConn {
    node_id: String,
    client: PunkFsClient<Channel>,
    last_seen: Mutex<Option<SystemTime>>,
}

impl NodeConn {
    fn touch(&self) {
        *self.last_seen.lock().unwrap() = Some(SystemTime::now());
    }

    fn reachable(&self, max_age: Duration) -> bool {
        self.last_seen
            .lock()
            .unwrap()
            .and_then(|seen| SystemTime::now().duration_since(seen).ok())
            .is_some_and(|age| age <= max_age)
    }
}

impl Store {
    /// Build a store over a single backing server.
    pub fn new(server_url: &str, node: FsNode) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_nodes(&[(node.node_id.clone(), server_url.to_owned())], node)
    }

    /// Build a store over several backing servers merged into one namespace.
    ///
    /// Params:
    /// - `nodes`: `(node_id, url)` pairs. **Order is significant**: it is
    ///   the stable precedence used when two nodes claim the same name.
    /// - `local`: this machine's identity, recorded on locally-tagged
    ///   entries.
    pub fn new_nodes(
        nodes: &[(String, String)],
        local: FsNode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_nodes_mode(nodes, local, false)
    }

    pub(crate) fn new_with_local(
        nodes: &[(String, String)],
        local: FsNode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_nodes_mode(nodes, local, true)
    }

    fn new_nodes_mode(
        nodes: &[(String, String)],
        local: FsNode,
        local_node_enabled: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;

        // connect_lazy doesn't fail if the server is down; the first RPC
        // connects. It needs a runtime context, hence enter().
        let conns = {
            let _guard = rt.enter();
            nodes
                .iter()
                .map(|(node_id, url)| {
                    let channel = Endpoint::from_shared(url.to_owned())?
                        .connect_timeout(Duration::from_secs(2))
                        .timeout(Duration::from_secs(2))
                        .connect_lazy();
                    Ok(NodeConn {
                        node_id: node_id.clone(),
                        client: PunkFsClient::new(channel),
                        last_seen: Mutex::new(None),
                    })
                })
                .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?
        };

        let mut metadata_cache = MetadataCache::new();
        // The root is synthetic: every node answers "/" with a different
        // guid, but a mount has exactly one root. Its children resolve per
        // node normally.
        metadata_cache.insert_root("/", "/", FsEntryKind::Directory);
        if local_node_enabled {
            metadata_cache.set_local_node(local.node_id.clone());
        }

        Ok(Store {
            metadata_cache: Mutex::new(metadata_cache),
            peer_generation: Mutex::new(HashMap::new()),
            listed: Mutex::new(HashSet::new()),
            rt,
            nodes: conns,
            node: local,
            conflicts: Mutex::new(HashSet::new()),
            local_node_enabled,
        })
    }

    /// Merge unsolicited metadata without treating missing entries as deletion.
    /// Conflicting contributions remain inaccessible rather than replacing a
    /// previously known owner. Caller validates the peer and all wire data.
    pub(crate) fn ingest_peer_entries(
        &self,
        ctx: &OperationContext,
        peer: &str,
        entries: Vec<FsEntry>,
    ) -> usize {
        let mut generations = self.peer_generation.lock().unwrap();
        let generation = generations.entry(peer.to_owned()).or_default();
        *generation = generation.wrapping_add(1);
        let mut cache = self.metadata_cache.lock().unwrap();
        let mut conflicts = Vec::new();
        let mut accepted = 0;
        for entry in entries {
            if let Some(known) = cache.get_entry_by_path(&entry.path) {
                if known.node_id != peer {
                    if known.kind == FsEntryKind::Directory && entry.kind == FsEntryKind::Directory
                    {
                        continue;
                    }
                    conflicts.push(entry.path.clone());
                    continue;
                }
                // Do not let a delayed announcement roll back fresher state.
                if known.guid != entry.guid {
                    conflicts.push(entry.path.clone());
                }
                continue;
            }
            if let Some(known) = cache.get_entry_by_guid(&entry.guid) {
                conflicts.push(entry.path.clone());
                conflicts.push(known.path.clone());
                continue;
            }
            cache.upsert_entry_in(ctx, entry);
            accepted += 1;
        }
        if !conflicts.is_empty() {
            self.conflicts.lock().unwrap().extend(conflicts);
        }
        drop(cache);
        drop(generations);
        accepted
    }

    /// How long a node may stay silent before its entries are treated as
    /// inaccessible. Overridable through the environment for tests.
    fn reachability_window() -> Duration {
        std::env::var("PUNKFS_NODE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(5))
    }

    /// The connection owning `node_id`.
    fn conn_for(&self, node_id: &str) -> Result<&NodeConn, StoreError> {
        self.nodes
            .iter()
            .find(|n| n.node_id == node_id)
            .ok_or_else(|| StoreError::Io(format!("unknown node {node_id}")))
    }

    /// Paths the last listing found claimed by more than one node.
    ///
    /// Exposed so a caller (or a test) can tell that a merge was
    /// ambiguous instead of trusting whichever entry won.
    pub fn conflicts(&self) -> Vec<String> {
        let mut known: Vec<String> = self.conflicts.lock().unwrap().iter().cloned().collect();
        known.sort();
        known
    }

    /// Which node owns `path`.
    fn node_of(&self, path: &str) -> Result<String, StoreError> {
        let tree = self.metadata_cache.lock().unwrap();
        if let Some(entry) = tree.get_entry_by_path(path) {
            return Ok(entry.node_id.clone());
        }
        // Legacy remote-only clients historically probed their first server
        // for unknown paths. A combined node must never guess an owner.
        if self.local_node_enabled {
            return Err(StoreError::NotFound);
        }
        self.nodes
            .first()
            .map(|node| node.node_id.clone())
            .ok_or_else(|| StoreError::Io("no backing nodes configured".into()))
    }

    /// The connection that should serve `path`.
    fn client_for(&self, path: &str) -> Result<(String, PunkFsClient<Channel>), StoreError> {
        let node_id = self.node_of(path)?;
        let conn = self.conn_for(&node_id)?;
        Ok((node_id.clone(), conn.client.clone()))
    }

    /// Ask every node to list `dir`, concurrently, and reconcile the union.
    ///
    /// Partial failure is normal: a node that does not answer contributes
    /// [`ListingOutcome::Unreachable`] and keeps its cached entries.
    fn fetch_listing(
        &self,
        ctx: &OperationContext,
        dir: &str,
        force: bool,
    ) -> Result<(), StoreError> {
        if !force && self.listed.lock().unwrap().contains(dir) {
            return Ok(());
        }

        let mut outcomes: Vec<(String, ListingOutcome)> = Vec::new();
        let mut any_answered = false;
        let observed_generations = self.peer_generation.lock().unwrap().clone();

        // Fetch in parallel under per-RPC deadlines. Preserve configuration
        // order when applying the outcomes so merging is deterministic.
        let answers = self.rt.block_on(async {
            let mut tasks = tokio::task::JoinSet::new();
            let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
            for (index, conn) in self.nodes.iter().enumerate() {
                let mut client = conn.client.clone();
                let request = ctx.request(ListDirectoryRequest { path: dir.into() });
                let permits = permits.clone();
                tasks.spawn(async move {
                    let _permit = permits.acquire().await.expect("semaphore remains open");
                    (index, client.list_directory(request).await)
                });
            }
            let mut answers: Vec<_> = (0..self.nodes.len()).map(|_| None).collect();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
            while let Ok(Some(result)) = tokio::time::timeout_at(deadline, tasks.join_next()).await
            {
                if let Ok((index, answer)) = result {
                    answers[index] = Some(answer);
                }
            }
            tasks.abort_all();
            answers
        });

        for (conn, answer) in self.nodes.iter().zip(answers) {
            match answer {
                Some(Ok(resp)) => {
                    conn.touch();
                    any_answered = true;
                    let children = resp
                        .into_inner()
                        .entries
                        .into_iter()
                        .map(|e| {
                            let full = join(dir, &e.name);
                            entry_from_attr(0, &full, e.attr.unwrap_or_default(), &conn.node_id)
                        })
                        .collect::<Vec<_>>();
                    outcomes.push((conn.node_id.clone(), ListingOutcome::Answered(children)));
                }
                Some(Err(_)) | None => {
                    outcomes.push((conn.node_id.clone(), ListingOutcome::Unreachable));
                }
            }
        }

        // Lock only after the network calls are done.
        // No configured remote nodes is normal in local-passthrough mode.
        if outcomes.is_empty() && self.local_node_enabled {
            return Ok(());
        }
        let generations = self.peer_generation.lock().unwrap();
        outcomes.retain(|(node, _)| {
            generations.get(node).copied().unwrap_or(0)
                == observed_generations.get(node).copied().unwrap_or(0)
        });
        let conflicts = self
            .metadata_cache
            .lock()
            .unwrap()
            .reconcile_from_in(ctx, dir, outcomes);
        drop(generations);
        if !conflicts.is_empty() {
            for path in &conflicts {
                eprintln!("punkfs: duplicate name across nodes, refusing to pick one: {path}");
            }
            let mut known = self.conflicts.lock().unwrap();
            known.extend(conflicts);
        }

        if any_answered {
            self.listed.lock().unwrap().insert(dir.to_owned());
            Ok(())
        } else {
            Err(StoreError::Io("no backing node answered".into()))
        }
    }

    /// Forget a cached directory so the next access refetches it.
    fn invalidate(&self, dir: &str) {
        self.listed.lock().unwrap().remove(dir);
    }

    /// Fetch one entry's attributes from the node that owns it.
    fn getattr(&self, ctx: &OperationContext, path: &str) -> Result<FileAttrData, StoreError> {
        let (node_id, mut client) = self.client_for(path)?;
        let resp = self
            .rt
            .block_on(client.get_attr(ctx.request(GetAttrRequest {
                path: path.to_owned(),
            })))
            .map_err(StoreError::from)?;
        self.conn_for(&node_id)?.touch();
        Ok(attr_from_proto(resp.into_inner().attr.unwrap_or_default()))
    }

    /// Store the attributes the server just reported for `path`.
    ///
    /// Ownership is preserved from the existing entry when we know it,
    /// so a refresh cannot silently re-home a file onto the local node.
    fn store_attr(&self, ctx: &OperationContext, path: &str, attr: FileAttrData) {
        let mut tree = self.metadata_cache.lock().unwrap();
        let existing = tree.get_entry_by_path(path).cloned();
        let inode = existing.as_ref().map(|e| e.inode).unwrap_or(0);
        let node_id = existing
            .map(|e| e.node_id)
            .unwrap_or_else(|| self.node.node_id.clone());
        tree.upsert_entry_in(
            ctx,
            FsEntry {
                inode,
                path: path.to_owned(),
                guid: attr.guid.clone(),
                kind: attr.kind,
                node_id,
                availability: Availability::Available,
                attr,
            },
        );
    }
}

/// The containing directory of a catalog path.
fn parent_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((head, _)) if !head.is_empty() => head.to_owned(),
        _ => "/".to_owned(),
    }
}

/// Join a directory and a child name into a catalog path.
fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

fn kind_from_proto(kind: i32) -> FsEntryKind {
    match punk_fs::FileKind::try_from(kind) {
        Ok(punk_fs::FileKind::Directory) => FsEntryKind::Directory,
        Ok(punk_fs::FileKind::Symlink) => FsEntryKind::Link,
        Ok(punk_fs::FileKind::File) => FsEntryKind::File,
        Ok(punk_fs::FileKind::Fifo) => FsEntryKind::Fifo,
        Ok(punk_fs::FileKind::Socket) => FsEntryKind::Socket,
        Ok(punk_fs::FileKind::BlockDevice) => FsEntryKind::BlockDevice,
        Ok(punk_fs::FileKind::CharDevice) => FsEntryKind::CharDevice,
        _ => FsEntryKind::Unknown,
    }
}

fn attr_from_proto(attr: punk_fs::FileAttr) -> FileAttrData {
    let times = attr.times.unwrap_or_default();
    let access = attr.access.unwrap_or_default();
    let stat = attr.stat.unwrap_or_default();

    FileAttrData {
        guid: attr.guid,
        kind: kind_from_proto(attr.kind),
        times: {
            let time = |secs: i64, nanos: u32| {
                let base = if secs >= 0 {
                    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(secs as u64))
                } else {
                    std::time::UNIX_EPOCH
                        .checked_sub(std::time::Duration::from_secs(secs.unsigned_abs()))
                };
                base.and_then(|t| t.checked_add(std::time::Duration::from_nanos(nanos as u64)))
                    .unwrap_or(std::time::UNIX_EPOCH)
            };
            FileTimes {
                accessed: time(times.accessed_unix, times.accessed_nanos),
                modified: time(times.modified_unix, times.modified_nanos),
                changed: time(times.changed_unix, times.changed_nanos),
                created: time(times.created_unix, times.created_nanos),
            }
        },
        access: Access {
            mode: access.mode as u16,
            uid: access.uid,
            gid: access.gid,
        },
        stat: Stat {
            size: stat.size,
            blocks: stat.blocks as u32,
            block_size: stat.block_size as u16,
            rdev: stat.rdev,
            nlink: stat.nlink,
            flags: stat.flags,
        },
    }
}

/// Build a catalog entry from a server-reported attribute message.
pub(crate) fn entry_from_attr(
    inode: u64,
    path: &str,
    attr: punk_fs::FileAttr,
    node_id: &str,
) -> FsEntry {
    let attr = attr_from_proto(attr);
    FsEntry {
        inode,
        path: path.to_owned(),
        guid: attr.guid.clone(),
        kind: attr.kind,
        node_id: node_id.to_owned(),
        availability: Availability::Available,
        attr,
    }
}

impl StoreApi for Store {
    fn reconcile_local_in(
        &self,
        ctx: &OperationContext,
        dir: &str,
        entries: Vec<FsEntry>,
    ) -> Result<(), StoreError> {
        let conflicts = self.metadata_cache.lock().unwrap().reconcile_from_in(
            ctx,
            dir,
            vec![(self.node.node_id.clone(), ListingOutcome::Answered(entries))],
        );
        if !conflicts.is_empty() {
            self.conflicts.lock().unwrap().extend(conflicts);
        }
        Ok(())
    }
    fn upsert_local_in(&self, ctx: &OperationContext, entry: FsEntry) -> Result<u64, StoreError> {
        if entry.node_id != self.node.node_id {
            return Err(StoreError::NotPermitted);
        }
        let mut cache = self.metadata_cache.lock().unwrap();
        let path = entry.path.clone();
        if cache
            .get_entry_by_path(&path)
            .is_some_and(|existing| existing.node_id != self.node.node_id)
        {
            return Err(StoreError::Local(libc::EEXIST));
        }
        cache.upsert_entry_in(ctx, entry);
        cache.get_inode(&path).ok_or(StoreError::NotFound)
    }
    fn remove_local_in(&self, ctx: &OperationContext, path: &str) -> Result<(), StoreError> {
        self.metadata_cache
            .lock()
            .unwrap()
            .remove_entry_in(ctx, path);
        self.invalidate(&parent_of(path));
        Ok(())
    }
    fn exchange_local_in(
        &self,
        _ctx: &OperationContext,
        old: &str,
        new: &str,
    ) -> Result<(), StoreError> {
        self.metadata_cache
            .lock()
            .unwrap()
            .exchange_entries(old, new);
        self.invalidate(&parent_of(old));
        self.invalidate(&parent_of(new));
        Ok(())
    }
    fn rename_local_in(
        &self,
        ctx: &OperationContext,
        old: &str,
        new: &str,
    ) -> Result<(), StoreError> {
        let mut cache = self.metadata_cache.lock().unwrap();
        if old != new {
            cache.remove_entry_in(ctx, new);
        }
        cache.rename_entry_in(ctx, old, new);
        drop(cache);
        self.invalidate(&parent_of(old));
        self.invalidate(&parent_of(new));
        Ok(())
    }
    fn has_conflict(&self, path: &str) -> bool {
        self.conflicts.lock().unwrap().contains(path)
    }
    fn has_remote_nodes(&self) -> bool {
        !self.nodes.is_empty()
    }
    fn node_id(&self) -> &str {
        &self.node.node_id
    }

    fn get_inode(&self, path: &str) -> Option<u64> {
        self.metadata_cache.lock().unwrap().get_inode(path)
    }

    fn get_path(&self, inode: u64) -> Option<String> {
        self.metadata_cache
            .lock()
            .unwrap()
            .get_path(inode)
            .map(str::to_owned)
    }

    fn get_entry(&self, inode: u64) -> Option<FsEntry> {
        self.metadata_cache
            .lock()
            .unwrap()
            .get_entry(inode)
            .cloned()
    }

    fn owner_of(&self, path: &str) -> Option<String> {
        self.metadata_cache
            .lock()
            .unwrap()
            .get_entry_by_path(path)
            .map(|e| e.node_id.clone())
            .filter(|id| !id.is_empty())
    }

    /// Fetch and reconcile `dir`.
    ///
    /// Always refetches rather than trusting a "we have listed this once"
    /// flag: the backing storage can change without us, and a stale
    /// listing would report entries that no longer exist (or hide new
    /// ones). Correctness first; caching belongs behind a validity check
    /// we do not have yet.
    fn ensure_listed(&self, dir: &str) -> Result<(), StoreError> {
        self.ensure_listed_in(&OperationContext::new(), dir)
    }

    fn ensure_listed_in(&self, ctx: &OperationContext, dir: &str) -> Result<(), StoreError> {
        self.fetch_listing(ctx, dir, true)
    }

    /// Children as (inode, name, kind). A server error is reported to the
    /// caller, which decides whether browsing may fall back to the cache.
    fn get_children(&self, path: &str) -> Vec<StoreChild> {
        self.get_children_in(&OperationContext::new(), path)
    }

    fn get_children_in(&self, ctx: &OperationContext, path: &str) -> Vec<StoreChild> {
        let _ = self.fetch_listing(ctx, path, true);
        self.cached_children(path)
    }

    fn cached_children(&self, path: &str) -> Vec<StoreChild> {
        self.metadata_cache
            .lock()
            .unwrap()
            .get_children_of(path)
            .into_iter()
            .map(|e| StoreChild {
                inode: e.inode,
                name: e.path.rsplit('/').next().unwrap_or("").to_owned(),
                kind: e.kind,
            })
            .collect()
    }

    fn readlink(&self, path: &str) -> Result<Vec<u8>, StoreError> {
        self.readlink_in(&OperationContext::new(), path)
    }

    fn readlink_in(&self, ctx: &OperationContext, path: &str) -> Result<Vec<u8>, StoreError> {
        self.require_reachable(ctx, path)?;
        let (node_id, mut client) = self.client_for(path)?;
        let response = self
            .rt
            .block_on(client.read_link(ctx.request(ReadLinkRequest {
                path: path.to_owned(),
            })))
            .map_err(|status| {
                if status.code() == tonic::Code::InvalidArgument
                    && status.message() == "not a symlink"
                {
                    StoreError::Local(libc::EINVAL)
                } else {
                    StoreError::from(status)
                }
            })?;
        self.conn_for(&node_id)?.touch();
        Ok(response.into_inner().target)
    }

    fn read(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>, StoreError> {
        self.read_in(&OperationContext::new(), path, offset, size)
    }

    fn read_in(
        &self,
        ctx: &OperationContext,
        path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, StoreError> {
        self.require_reachable(ctx, path)?;
        let (node_id, mut client) = self.client_for(path)?;
        let resp = self
            .rt
            .block_on(client.read_file(ctx.request(ReadFileRequest {
                path: path.to_owned(),
                offset,
                length: size,
            })))
            .map_err(StoreError::from)?;
        self.conn_for(&node_id)?.touch();
        Ok(resp.into_inner().data)
    }

    fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<u32, StoreError> {
        self.write_in(&OperationContext::new(), path, offset, data)
    }

    fn write_in(
        &self,
        ctx: &OperationContext,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<u32, StoreError> {
        self.require_reachable(ctx, path)?;
        let (node_id, mut client) = self.client_for(path)?;
        let resp = self
            .rt
            .block_on(client.write_file(ctx.request(WriteFileRequest {
                path: path.to_owned(),
                offset,
                data: data.to_vec(),
            })))
            .map_err(StoreError::from)?;
        self.conn_for(&node_id)?.touch();
        Ok(resp.into_inner().written)
    }

    fn fsync(&self, path: &str, datasync: bool) -> Result<(), StoreError> {
        self.fsync_in(&OperationContext::new(), path, datasync)
    }

    fn fsync_in(
        &self,
        ctx: &OperationContext,
        path: &str,
        datasync: bool,
    ) -> Result<(), StoreError> {
        self.require_reachable(ctx, path)?;
        let (node_id, mut client) = self.client_for(path)?;
        self.rt
            .block_on(client.fsync_file(ctx.request(FsyncFileRequest {
                path: path.to_owned(),
                datasync,
            })))
            .map_err(StoreError::from)?;
        self.conn_for(&node_id)?.touch();
        Ok(())
    }

    fn get_xattr(&self, path: &str, name: &[u8]) -> Result<Vec<u8>, StoreError> {
        self.get_xattr_in(&OperationContext::new(), path, name)
    }
    fn list_xattr(&self, path: &str) -> Result<Vec<u8>, StoreError> {
        self.list_xattr_in(&OperationContext::new(), path)
    }
    fn set_xattr(
        &self,
        path: &str,
        name: &[u8],
        value: &[u8],
        flags: i32,
    ) -> Result<(), StoreError> {
        self.set_xattr_in(&OperationContext::new(), path, name, value, flags)
    }
    fn remove_xattr(&self, path: &str, name: &[u8]) -> Result<(), StoreError> {
        self.remove_xattr_in(&OperationContext::new(), path, name)
    }
    fn get_xattr_in(
        &self,
        ctx: &OperationContext,
        path: &str,
        name: &[u8],
    ) -> Result<Vec<u8>, StoreError> {
        self.require_reachable(ctx, path)?;
        let (node, mut client) = self.client_for(path)?;
        let request = punk_fs::XattrRequest {
            path: path.into(),
            name: name.into(),
            value: vec![],
            flags: 0,
        };
        let response = self
            .rt
            .block_on(client.get_xattr(ctx.request(request)))
            .map_err(StoreError::from)?;
        self.conn_for(&node)?.touch();
        Ok(response.into_inner().value)
    }
    fn list_xattr_in(&self, ctx: &OperationContext, path: &str) -> Result<Vec<u8>, StoreError> {
        self.require_reachable(ctx, path)?;
        let (node, mut client) = self.client_for(path)?;
        let request = punk_fs::XattrRequest {
            path: path.into(),
            name: vec![],
            value: vec![],
            flags: 0,
        };
        let response = self
            .rt
            .block_on(client.list_xattr(ctx.request(request)))
            .map_err(StoreError::from)?;
        self.conn_for(&node)?.touch();
        Ok(response.into_inner().value)
    }
    fn set_xattr_in(
        &self,
        ctx: &OperationContext,
        path: &str,
        name: &[u8],
        value: &[u8],
        flags: i32,
    ) -> Result<(), StoreError> {
        self.require_reachable(ctx, path)?;
        let (node, mut client) = self.client_for(path)?;
        let request = punk_fs::XattrRequest {
            path: path.into(),
            name: name.into(),
            value: value.into(),
            flags,
        };
        self.rt
            .block_on(client.set_xattr(ctx.request(request)))
            .map_err(StoreError::from)?;
        self.conn_for(&node)?.touch();
        Ok(())
    }
    fn remove_xattr_in(
        &self,
        ctx: &OperationContext,
        path: &str,
        name: &[u8],
    ) -> Result<(), StoreError> {
        self.require_reachable(ctx, path)?;
        let (node, mut client) = self.client_for(path)?;
        let request = punk_fs::XattrRequest {
            path: path.into(),
            name: name.into(),
            value: vec![],
            flags: 0,
        };
        self.rt
            .block_on(client.remove_xattr(ctx.request(request)))
            .map_err(StoreError::from)?;
        self.conn_for(&node)?.touch();
        Ok(())
    }

    fn setattr(&self, path: &str, changes: SetattrChanges) -> Result<FileAttrData, StoreError> {
        self.setattr_in(&OperationContext::new(), path, changes)
    }

    fn setattr_in(
        &self,
        ctx: &OperationContext,
        path: &str,
        changes: SetattrChanges,
    ) -> Result<FileAttrData, StoreError> {
        self.require_reachable(ctx, path)?;
        let (node_id, mut client) = self.client_for(path)?;
        let resp = self
            .rt
            .block_on(client.set_attr(ctx.request(SetAttrRequest {
                path: path.to_owned(),
                mode: changes.mode,
                size: changes.size,
                accessed_unix: changes.accessed_unix,
                modified_unix: changes.modified_unix,
                uid: changes.uid,
                gid: changes.gid,
                accessed_nanos: changes.accessed_nanos,
                modified_nanos: changes.modified_nanos,
            })))
            .map_err(StoreError::from)?;
        self.conn_for(&node_id)?.touch();

        let attr = attr_from_proto(resp.into_inner().attr.unwrap_or_default());
        self.store_attr(ctx, path, attr.clone());
        Ok(attr)
    }

    fn refresh(&self, path: &str) -> Result<FileAttrData, StoreError> {
        self.refresh_in(&OperationContext::new(), path)
    }

    fn refresh_in(&self, ctx: &OperationContext, path: &str) -> Result<FileAttrData, StoreError> {
        let attr = self.getattr(ctx, path)?;
        self.store_attr(ctx, path, attr.clone());
        Ok(attr)
    }

    fn statfs(&self) -> Result<StatFs, StoreError> {
        self.statfs_in(&OperationContext::new())
    }

    fn statfs_in(&self, ctx: &OperationContext) -> Result<StatFs, StoreError> {
        // Capacity is reported by the first reachable node: an aggregate
        // would claim space no single node can actually serve.
        let mut resp = None;
        for conn in &self.nodes {
            let mut client = conn.client.clone();
            if let Ok(answer) = self
                .rt
                .block_on(client.stat_fs(ctx.request(StatFsRequest {})))
            {
                conn.touch();
                resp = Some(answer.into_inner());
                break;
            }
        }
        let resp = resp.ok_or_else(|| StoreError::Io("no backing node answered".into()))?;
        Ok(StatFs {
            blocks: resp.blocks,
            bfree: resp.bfree,
            bavail: resp.bavail,
            files: resp.files,
            ffree: resp.ffree,
            block_size: resp.block_size,
            name_length: resp.name_length,
            fragment_size: resp.fragment_size,
        })
    }

    fn unlink(&self, path: &str) -> Result<(), StoreError> {
        self.unlink_in(&OperationContext::new(), path)
    }

    fn unlink_in(&self, ctx: &OperationContext, path: &str) -> Result<(), StoreError> {
        self.require_reachable(ctx, path)?;
        let (node_id, mut client) = self.client_for(path)?;
        self.rt
            .block_on(client.remove_file(ctx.request(punk_fs::RemoveFileRequest {
                path: path.to_owned(),
            })))
            .map_err(StoreError::from)?;
        self.conn_for(&node_id)?.touch();
        self.after_mutation(ctx, path);
        Ok(())
    }

    fn rmdir(&self, path: &str) -> Result<(), StoreError> {
        self.rmdir_in(&OperationContext::new(), path)
    }

    fn rmdir_in(&self, ctx: &OperationContext, path: &str) -> Result<(), StoreError> {
        self.require_reachable(ctx, path)?;
        let (node_id, mut client) = self.client_for(path)?;
        self.rt
            .block_on(client.remove_dir(ctx.request(punk_fs::RemoveDirRequest {
                path: path.to_owned(),
            })))
            .map_err(StoreError::from)?;
        self.conn_for(&node_id)?.touch();
        self.after_mutation(ctx, path);
        Ok(())
    }

    fn rename(&self, old: &str, new: &str, flags: RenameFlags) -> Result<(), StoreError> {
        self.rename_in(&OperationContext::new(), old, new, flags)
    }

    fn rename_in(
        &self,
        ctx: &OperationContext,
        old: &str,
        new: &str,
        flags: RenameFlags,
    ) -> Result<(), StoreError> {
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            let cache = self.metadata_cache.lock().unwrap();
            let source = cache.get_entry_by_path(old).ok_or(StoreError::NotFound)?;
            let destination = cache.get_entry_by_path(new).ok_or(StoreError::NotFound)?;
            if source.node_id != destination.node_id {
                return Err(StoreError::Local(libc::EXDEV));
            }
            drop(cache);
            if self.has_conflict(old) || self.has_conflict(new) {
                return Err(StoreError::Local(libc::EIO));
            }
        }
        self.require_reachable(ctx, old)?;
        let (node_id, mut client) = self.client_for(old)?;
        self.rt
            .block_on(client.rename(ctx.request(punk_fs::RenameRequest {
                old_path: old.to_owned(),
                new_path: new.to_owned(),
                flags: flags.bits(),
            })))
            .map_err(StoreError::from)?;
        self.conn_for(&node_id)?.touch();

        // Rename within one node: the entry keeps its guid and its inode,
        // so move it locally instead of dropping and re-adding it. A
        // cross-node move is refused by the op layer, never reaching here.
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            self.metadata_cache
                .lock()
                .unwrap()
                .exchange_entries(old, new);
            self.invalidate(&parent_of(new));
        } else {
            self.after_mutation(ctx, new);
            self.metadata_cache
                .lock()
                .unwrap()
                .rename_entry_in(ctx, old, new);
        }
        let parent = parent_of(old);
        self.invalidate(&parent);
        Ok(())
    }
}

impl Store {
    /// Refuse an operation that needs bytes from a node we cannot reach.
    ///
    /// This is what makes "node down" cheap instead of pathological: we
    /// answer from `last_seen` rather than paying a connect timeout on
    /// every `cat`. Browsing (`ls`, `stat`) deliberately does not call this
    /// — the namespace stays visible while the content is not.
    fn require_reachable(&self, ctx: &OperationContext, path: &str) -> Result<(), StoreError> {
        let node_id = self.node_of(path)?;
        let conn = self.conn_for(&node_id)?;
        if conn.reachable(Self::reachability_window()) {
            return Ok(());
        }
        // Never seen, or silent for too long: one cheap retry, because a
        // node that just came back should not need a second syscall.
        let mut client = conn.client.clone();
        let alive = self
            .rt
            .block_on(client.get_attr(ctx.request(GetAttrRequest {
                path: path.to_owned(),
            })))
            .is_ok();
        if alive {
            conn.touch();
            Ok(())
        } else {
            Err(StoreError::Inaccessible(node_id))
        }
    }

    /// Update the local snapshot after a successful mutation of `path`.
    ///
    /// The entry is dropped (or, for a directory, its children are) and the
    /// containing directory is invalidated so the next listing refetches.
    fn after_mutation(&self, ctx: &OperationContext, path: &str) {
        let parent = parent_of(path);
        self.invalidate(&parent);
        self.metadata_cache
            .lock()
            .unwrap()
            .remove_entry_in(ctx, path);
    }
}
