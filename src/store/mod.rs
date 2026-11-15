//! The client-side store: the only way the filesystem layer reaches the
//! catalog.
//!
//! It owns the local snapshot of the server's tree ([`FsTree`]), the grpc
//! client, and the tokio runtime used to drive async calls from the
//! synchronous FUSE callbacks.
//!
//! Identity note: entries are identified by `uuid` (the punkfs identity,
//! assigned by the server) and addressed locally by `inode` (allocated
//! here). The store is the layer that maps between the two.
use std::collections::HashSet;
use std::sync::Mutex;


use fuser::RenameFlags;
use tokio::runtime::Runtime;
use tonic::transport::{Channel, Endpoint};

use crate::db::fstree::{Access, FsEntry, FsEntryKind, FsNode, FsTree, FileAttrData, FileTimes, Stat};
use crate::punk_fs::punk_fs_client::PunkFsClient;
use crate::punk_fs::{
    self, FsyncFileRequest, GetAttrRequest, ListDirectoryRequest, ReadFileRequest, SetAttrRequest,
    StatFsRequest, WriteFileRequest,
};

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
    /// Anything from the transport or the server.
    Io(String),
}

impl From<tonic::Status> for StoreError {
    fn from(status: tonic::Status) -> Self {
        match status.code() {
            tonic::Code::NotFound => StoreError::NotFound,
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
/// `None` means "leave this alone". The fields mirror the proto
/// `SetAttrRequest`; times are already resolved to concrete seconds by the
/// op layer, so the store never sees `TimeOrNow`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetattrChanges {
    pub mode: Option<u32>,
    pub size: Option<u64>,
    pub accessed_unix: Option<i64>,
    pub modified_unix: Option<i64>,
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
/// every method can be unit tested against [`mock::MockStore`] without a
/// server, a runtime, or a FUSE session.
pub trait StoreApi: Send + Sync {
    /// This machine's identity, as registered with the server.
    fn node_id(&self) -> &str;

    // --- catalog -------------------------------------------------------
    /// Inode for `path`, if the catalog knows it.
    fn get_inode(&self, path: &str) -> Option<u64>;
    /// Path for `inode`, if the catalog knows it.
    fn get_path(&self, inode: u64) -> Option<String>;
    /// The whole entry (kind, metadata, path, uuid) for an inode.
    fn get_entry(&self, inode: u64) -> Option<FsEntry>;
    /// Fetch and reconcile `dir` from the server.
    fn ensure_listed(&self, dir: &str) -> Result<(), StoreError>;
    /// Children of `path`, using whatever listing is cached.
    fn get_children(&self, path: &str) -> Vec<StoreChild>;

    // --- content -------------------------------------------------------
    /// Read at most `size` bytes at `offset`; a short read means EOF.
    fn read(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>, StoreError>;
    /// Write `data` at `offset`; returns how many bytes were taken.
    fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<u32, StoreError>;
    /// Durability barrier for `path`; `datasync` skips metadata.
    fn fsync(&self, path: &str, datasync: bool) -> Result<(), StoreError>;

    // --- metadata ------------------------------------------------------
    /// Apply the requested attribute changes, returning the new attributes.
    fn setattr(&self, path: &str, changes: SetattrChanges) -> Result<FileAttrData, StoreError>;
    /// Re-read one entry's attributes, bypassing the cache.
    fn refresh(&self, path: &str) -> Result<FileAttrData, StoreError>;
    /// Aggregate capacity for `statfs`.
    fn statfs(&self) -> Result<StatFs, StoreError>;

    // --- namespace mutation --------------------------------------------
    // Creation is deliberately absent: entries are created by the machine
    // that owns the storage, never through the merged mount.
    /// Remove the file at `path`.
    fn unlink(&self, path: &str) -> Result<(), StoreError>;
    /// Remove the *empty* directory at `path`.
    fn rmdir(&self, path: &str) -> Result<(), StoreError>;
    /// Move `old` to `new`, honouring `flags`.
    fn rename(&self, old: &str, new: &str, flags: RenameFlags) -> Result<(), StoreError>;
}

pub struct Store {
    fstree: Mutex<FsTree>,
    listed: Mutex<HashSet<String>>, // directories already fetched from the server
    rt: Runtime,
    client: PunkFsClient<Channel>,
    node: FsNode, // where am i running right now
}

impl Store {
    pub fn new(server_url: &str, node: FsNode) -> Result<Self, Box<dyn std::error::Error>> {
        let rt = Runtime::new()?;
        // connect_lazy doesn't fail if the server is down; the first RPC
        // connects. It needs a runtime context, hence enter().
        let channel = {
            let _guard = rt.enter();
            Endpoint::from_shared(server_url.to_owned())?.connect_lazy()
        };

        let mut fstree = FsTree::new();
        // The root's uuid is the server's answer for "/"; it is refreshed by
        // the first listing, so any placeholder would do. A fixed string
        // keeps it stable across restarts.
        fstree.insert_root("/", "/", FsEntryKind::Directory);

        Ok(Store {
            fstree: Mutex::new(fstree),
            listed: Mutex::new(HashSet::new()),
            rt,
            client: PunkFsClient::new(channel),
            node,
        })
    }

    /// Fetch `dir` from the server unless we already have.
    ///
    /// On success the cached children are reconciled against the answer,
    /// which is what makes a missing child a genuine deletion.
    fn fetch_listing(&self, dir: &str, force: bool) -> Result<(), StoreError> {
        if !force && self.listed.lock().unwrap().contains(dir) {
            return Ok(());
        }

        let mut client = self.client.clone(); // cheap: shares the channel
        let resp = self
            .rt
            .block_on(client.list_directory(ListDirectoryRequest { path: dir.into() }))?
            .into_inner();

        let children = resp
            .entries
            .into_iter()
            .map(|e| {
                let full = join(dir, &e.name);
                entry_from_attr(0, &full, e.attr.unwrap_or_default(), &self.node.node_id)
            })
            .collect::<Vec<_>>();

        // Lock only after the network call is done.
        self.fstree.lock().unwrap().reconcile(dir, children);
        self.listed.lock().unwrap().insert(dir.to_owned());
        Ok(())
    }

    /// Forget a cached directory so the next access refetches it.
    fn invalidate(&self, dir: &str) {
        self.listed.lock().unwrap().remove(dir);
    }

    fn client(&self) -> PunkFsClient<Channel> {
        self.client.clone()
    }

    fn getattr(&self, path: &str) -> Result<FileAttrData, StoreError> {
        let mut client = self.client();
        let resp = self
            .rt
            .block_on(client.get_attr(GetAttrRequest {
                path: path.to_owned(),
            }))?
            .into_inner();
        Ok(attr_from_proto(resp.attr.unwrap_or_default()))
    }

    /// Store the attributes the server just reported for `path`.
    fn store_attr(&self, path: &str, attr: FileAttrData) {
        let mut tree = self.fstree.lock().unwrap();
        let inode = tree.get_inode(path).unwrap_or(0);
        tree.upsert_entry(FsEntry {
            inode,
            path: path.to_owned(),
            uuid: attr.uuid.clone(),
            kind: attr.kind,
            node_id: self.node.node_id.clone(),
            attr,
        });
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
        _ => FsEntryKind::Unknown,
    }
}

fn attr_from_proto(attr: punk_fs::FileAttr) -> FileAttrData {
    let times = attr.times.unwrap_or_default();
    let access = attr.access.unwrap_or_default();
    let stat = attr.stat.unwrap_or_default();

    FileAttrData {
        uuid: attr.uuid,
        kind: kind_from_proto(attr.kind),
        times: FileTimes::from_secs(
            times.accessed_unix.max(0) as u64,
            times.modified_unix.max(0) as u64,
            times.changed_unix.max(0) as u64,
            times.created_unix.max(0) as u64,
        ),
        access: Access {
            mode: access.mode as u16,
            uid: access.uid,
            gid: access.gid,
        },
        stat: Stat {
            size: stat.size,
            blocks: stat.blocks as u32,
            block_size: stat.block_size as u16,
            rdev: stat.rdev as u16,
            nlink: stat.nlink,
            flags: stat.flags,
        },
    }
}

/// Build a catalog entry from a server-reported attribute message.
fn entry_from_attr(inode: u64, path: &str, attr: punk_fs::FileAttr, node_id: &str) -> FsEntry {
    let attr = attr_from_proto(attr);
    FsEntry {
        inode,
        path: path.to_owned(),
        uuid: attr.uuid.clone(),
        kind: attr.kind,
        node_id: node_id.to_owned(),
        attr,
    }
}

impl StoreApi for Store {
    fn node_id(&self) -> &str {
        &self.node.node_id
    }

    fn get_inode(&self, path: &str) -> Option<u64> {
        self.fstree.lock().unwrap().get_inode(path)
    }

    fn get_path(&self, inode: u64) -> Option<String> {
        self.fstree
            .lock()
            .unwrap()
            .get_path(inode)
            .map(str::to_owned)
    }

    fn get_entry(&self, inode: u64) -> Option<FsEntry> {
        self.fstree.lock().unwrap().get_entry(inode).cloned()
    }

    /// Fetch and reconcile `dir`.
    ///
    /// Always refetches rather than trusting a "we have listed this once"
    /// flag: the backing storage can change without us, and a stale
    /// listing would report entries that no longer exist (or hide new
    /// ones). Correctness first; caching belongs behind a validity check
    /// we do not have yet.
    fn ensure_listed(&self, dir: &str) -> Result<(), StoreError> {
        self.fetch_listing(dir, true)
    }

    /// Children as (inode, name, kind). A server error is reported to the
    /// caller, which decides whether browsing may fall back to the cache.
    fn get_children(&self, path: &str) -> Vec<StoreChild> {
        let _ = self.fetch_listing(path, true);

        self.fstree
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

    fn read(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>, StoreError> {
        let mut client = self.client();
        let resp = self
            .rt
            .block_on(client.read_file(ReadFileRequest {
                path: path.to_owned(),
                offset,
                length: size,
            }))?
            .into_inner();
        Ok(resp.data)
    }

    fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<u32, StoreError> {
        let mut client = self.client();
        let resp = self
            .rt
            .block_on(client.write_file(WriteFileRequest {
                path: path.to_owned(),
                offset,
                data: data.to_vec(),
            }))?
            .into_inner();
        Ok(resp.written)
    }

    fn fsync(&self, path: &str, datasync: bool) -> Result<(), StoreError> {
        let mut client = self.client();
        self.rt.block_on(client.fsync_file(FsyncFileRequest {
            path: path.to_owned(),
            datasync,
        }))?;
        Ok(())
    }

    fn setattr(&self, path: &str, changes: SetattrChanges) -> Result<FileAttrData, StoreError> {
        let mut client = self.client();
        let resp = self
            .rt
            .block_on(client.set_attr(SetAttrRequest {
                path: path.to_owned(),
                mode: changes.mode,
                size: changes.size,
                accessed_unix: changes.accessed_unix,
                modified_unix: changes.modified_unix,
                uid: None,
                gid: None,
            }))?
            .into_inner();

        let attr = attr_from_proto(resp.attr.unwrap_or_default());
        self.store_attr(path, attr.clone());
        Ok(attr)
    }

    fn refresh(&self, path: &str) -> Result<FileAttrData, StoreError> {
        let attr = self.getattr(path)?;
        self.store_attr(path, attr.clone());
        Ok(attr)
    }

    fn statfs(&self) -> Result<StatFs, StoreError> {
        let mut client = self.client();
        let resp = self
            .rt
            .block_on(client.stat_fs(StatFsRequest {}))?
            .into_inner();
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
        let mut client = self.client();
        self.rt
            .block_on(client.remove_file(punk_fs::RemoveFileRequest {
                path: path.to_owned(),
            }))?;
        self.after_mutation(path);
        Ok(())
    }

    fn rmdir(&self, path: &str) -> Result<(), StoreError> {
        let mut client = self.client();
        self.rt.block_on(client.remove_dir(punk_fs::RemoveDirRequest {
            path: path.to_owned(),
        }))?;
        self.after_mutation(path);
        Ok(())
    }

    fn rename(&self, old: &str, new: &str, flags: RenameFlags) -> Result<(), StoreError> {
        let mut client = self.client();
        self.rt.block_on(client.rename(punk_fs::RenameRequest {
            old_path: old.to_owned(),
            new_path: new.to_owned(),
            flags: flags.bits(),
        }))?;

        self.after_mutation(old);
        self.after_mutation(new);
        self.fstree.lock().unwrap().rename_entry(old, new);
        Ok(())
    }
}

impl Store {
    /// Update the local snapshot after a successful mutation of `path`.
    ///
    /// The entry is dropped (or, for a directory, its children are) and the
    /// containing directory is invalidated so the next listing refetches.
    fn after_mutation(&self, path: &str) {
        let parent = match path.rsplit_once('/') {
            Some((head, _)) if !head.is_empty() => head.to_owned(),
            _ => "/".to_owned(),
        };
        self.invalidate(&parent);
        self.fstree.lock().unwrap().remove_entry(path);
    }
}
