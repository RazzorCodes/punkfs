use std::collections::HashSet;
use std::sync::Mutex;
use std::time::SystemTime;

use fuser::OpenFlags;
use fuser::RenameFlags;
use tokio::runtime::Runtime;
use tonic::transport::{Channel, Endpoint};

use crate::db::fstree::{FsEntry, FsEntryKind, FsMetadata, FsNode, FsTree};
use crate::punk_fs::{FileKind, ListDirectoryRequest};
use crate::punk_fs::punk_fs_client::PunkFsClient;

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
    /// The catalog refuses the operation (policy, offline owner, ...).
    NotPermitted,
    /// No rpc / no implementation exists yet.
    Unimplemented,
    /// Anything from the transport or the server.
    Io(String),
}

impl From<tonic::Status> for StoreError {
    fn from(status: tonic::Status) -> Self {
        match status.code() {
            tonic::Code::NotFound => StoreError::NotFound,
            tonic::Code::PermissionDenied => StoreError::NotPermitted,
            tonic::Code::Unimplemented => StoreError::Unimplemented,
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

/// Requested attribute changes from one `setattr` call. `None` = leave
/// the field alone. This is the FUSE op collapsed into plain values, so
/// the op layer does not have to pass `TimeOrNow`/`BsdFileFlags` down.
///
/// The fields are the contract the write path will consume; the store
/// stub does not read them yet.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetattrChanges {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<SystemTime>,
    pub mtime: Option<SystemTime>,
    pub ctime: Option<SystemTime>,
    pub crtime: Option<SystemTime>,
    pub chgtime: Option<SystemTime>,
    pub bkuptime: Option<SystemTime>,
    pub flags: Option<u32>,
}

/// Filesystem-wide capacity counters backing `statfs` / `df`.
///
/// Per spec §5 an offline node contributes last-known, not zero values;
/// that decision lives in the store implementation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatFs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
}

/// The filesystem's only view of the catalog.
///
/// The op modules (`crate::filesystem::*`) take this as a trait object so
/// every method can be unit tested against [`mock::MockStore`] without a
/// server, a runtime, or a FUSE session.
///
/// Read methods are implemented by [`Store`]; everything that mutates
/// state (or moves bytes) is a stub returning
/// [`StoreError::Unimplemented`] until the corresponding rpc exists.
pub trait StoreApi: Send + Sync {
    /// This machine's identity, as registered with the server. Needed
    /// once mutations must state which node is authoritative.
    #[allow(dead_code)]
    fn node_id(&self) -> &str;

    // --- read side -----------------------------------------------------
    /// Inode for `path`, if the catalog knows it.
    fn get_inode(&self, path: &str) -> Option<u64>;
    /// Path for `inode`, if the catalog knows it.
    fn get_path(&self, inode: u64) -> Option<String>;
    /// The whole entry (kind, metadata, path, owner) for an inode.
    fn get_entry(&self, inode: u64) -> Option<FsEntry>;
    /// Fetch `dir` from the server unless already fetched. Safe to call
    /// repeatedly; the first failure is reported, later calls retry.
    fn ensure_listed(&self, dir: &str) -> Result<(), StoreError>;
    /// Children of `path`, using whatever listing is cached.
    fn get_children(&self, path: &str) -> Vec<StoreChild>;

    // --- open-file lifecycle -------------------------------------------
    /// Open `path` and return a handle for the later `read`/`write`.
    fn open(&self, path: &str, flags: OpenFlags) -> Result<u64, StoreError>;
    /// Read at most `size` bytes at `offset`; short reads mean EOF.
    fn read(&self, path: &str, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, StoreError>;
    /// Write `data` at `offset`; returns how many bytes were taken.
    fn write(&self, path: &str, fh: u64, offset: u64, data: &[u8]) -> Result<u32, StoreError>;
    /// Push buffered state for `fh` out to the owning node.
    fn flush(&self, path: &str, fh: u64) -> Result<(), StoreError>;
    /// Drop the handle; called exactly once per `open`.
    fn release(&self, path: &str, fh: u64) -> Result<(), StoreError>;
    /// Durability barrier for `fh`; `datasync` skips metadata.
    fn fsync(&self, path: &str, fh: u64, datasync: bool) -> Result<(), StoreError>;

    // --- namespace / metadata mutation ---------------------------------
    /// Apply the requested attribute changes, returning the new metadata.
    fn setattr(&self, path: &str, changes: SetattrChanges) -> Result<FsMetadata, StoreError>;
    /// Create a directory at `path`; returns its inode.
    fn mkdir(&self, path: &str, mode: u32) -> Result<u64, StoreError>;
    /// Remove the file at `path`.
    fn unlink(&self, path: &str) -> Result<(), StoreError>;
    /// Remove the *empty* directory at `path`.
    fn rmdir(&self, path: &str) -> Result<(), StoreError>;
    /// Move `old` to `new`, honouring `flags` (noreplace/exchange).
    fn rename(&self, old: &str, new: &str, flags: RenameFlags) -> Result<(), StoreError>;
    /// Aggregate capacity for `statfs`.
    fn statfs(&self) -> Result<StatFs, StoreError>;
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
        // connect_lazy doesn't fail if the server is down; the first RPC connects.
        // It needs a runtime context, hence enter().
        let channel = {
            let _guard = rt.enter();
            Endpoint::from_shared(server_url.to_owned())?.connect_lazy()
        };

        let fstree = FsTree { entries: vec![] };
        let store = Store {
            fstree: Mutex::new(fstree),
            listed: Mutex::new(HashSet::new()),
            rt,
            client: PunkFsClient::new(channel),
            node,
        };
        store.upsert_entry("/", 1, FsEntryKind::Directory, None); // root
        Ok(store)
    }

    pub fn upsert_entry(
        &self,
        path: &str,
        inode: u64,
        kind: FsEntryKind,
        metadata: Option<FsMetadata>,
    ) -> Option<(String, u64)> {
        let old = self
            .fstree
            .lock()
            .unwrap()
            .upsert_entry(path, inode, kind, metadata)?;
        Some((old.path, old.inode))
    }

    /// Fetch `dir` from the server unless we already have. Safe to call repeatedly.
    ///
    /// This is the only method here that talks to the server today; every
    /// other implemented method answers from the local snapshot.
    fn fetch_listing(&self, dir: &str) -> Result<(), StoreError> {
        if self.listed.lock().unwrap().contains(dir) {
            return Ok(());
        }

        let mut client = self.client.clone(); // cheap: shares the channel
        let resp = self
            .rt
            .block_on(client.list_directory(ListDirectoryRequest { path: dir.into() }))?
            .into_inner();

        // lock only after the network call is done
        let mut tree = self.fstree.lock().unwrap();
        for e in resp.entries {
            let full = if dir == "/" {
                format!("/{}", e.name)
            } else {
                format!("{}/{}", dir.trim_end_matches('/'), e.name)
            };
            // The DirEntry carries (kind, size, mtime); fold all of it into
            // the stored entry so getattr can serve real values.
            let kind = match FileKind::try_from(e.kind) {
                Ok(FileKind::Directory) => FsEntryKind::Directory,
                Ok(FileKind::Symlink) => FsEntryKind::Link,
                _ => FsEntryKind::File, // File and Unspecified
            };
            let metadata = FsMetadata {
                size: e.size,
                mtime: e.mtime_unix.max(0) as u64,
                ..FsMetadata::default()
            };
            tree.upsert_entry(&full, 0, kind, Some(metadata)); // 0 = allocate, or reuse if the path exists
        }
        drop(tree);

        self.listed.lock().unwrap().insert(dir.to_owned());
        Ok(())
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
        let tree = self.fstree.lock().unwrap();
        tree.entries
            .iter()
            .find(|e| e.inode == inode)
            .cloned()
    }

    fn ensure_listed(&self, dir: &str) -> Result<(), StoreError> {
        self.fetch_listing(dir)
    }

    /// Children as (inode, name, kind). On a server error we serve whatever
    /// is cached: browsing offline is a spec requirement, not an error.
    fn get_children(&self, path: &str) -> Vec<StoreChild> {
        let _ = self.fetch_listing(path);

        let tree = self.fstree.lock().unwrap();
        tree.get_children_of(path)
            .into_iter()
            .map(|e| StoreChild {
                inode: e.inode,
                name: e.path.rsplit('/').next().unwrap_or("").to_owned(),
                kind: e.kind,
            })
            .collect()
    }

    // --- stubs: no rpc exists yet --------------------------------------
    // Each of these becomes a real client call once the corresponding
    // list_directory-style rpc lands; the trait shape above is the contract
    // the filesystem layer already codes against.

    fn open(&self, _path: &str, _flags: OpenFlags) -> Result<u64, StoreError> {
        Err(StoreError::Unimplemented) // TODO: Open/GetFile rpc
    }

    fn read(&self, _path: &str, _fh: u64, _offset: u64, _size: u32) -> Result<Vec<u8>, StoreError> {
        Err(StoreError::Unimplemented) // TODO: Read rpc
    }

    fn write(&self, _path: &str, _fh: u64, _offset: u64, _data: &[u8]) -> Result<u32, StoreError> {
        Err(StoreError::Unimplemented) // TODO: Write rpc
    }

    fn flush(&self, _path: &str, _fh: u64) -> Result<(), StoreError> {
        Err(StoreError::Unimplemented) // TODO: Flush rpc
    }

    fn release(&self, _path: &str, _fh: u64) -> Result<(), StoreError> {
        Err(StoreError::Unimplemented) // TODO: Release rpc
    }

    fn fsync(&self, _path: &str, _fh: u64, _datasync: bool) -> Result<(), StoreError> {
        Err(StoreError::Unimplemented) // TODO: Fsync rpc
    }

    fn setattr(&self, _path: &str, _changes: SetattrChanges) -> Result<FsMetadata, StoreError> {
        Err(StoreError::Unimplemented) // TODO: Setattr rpc
    }

    fn mkdir(&self, _path: &str, _mode: u32) -> Result<u64, StoreError> {
        Err(StoreError::Unimplemented) // TODO: Mkdir rpc
    }

    fn unlink(&self, _path: &str) -> Result<(), StoreError> {
        Err(StoreError::Unimplemented) // TODO: Unlink rpc (must refuse offline owner, spec §9)
    }

    fn rmdir(&self, _path: &str) -> Result<(), StoreError> {
        Err(StoreError::Unimplemented) // TODO: Rmdir rpc
    }

    fn rename(&self, _old: &str, _new: &str, _flags: RenameFlags) -> Result<(), StoreError> {
        Err(StoreError::Unimplemented) // TODO: Rename rpc (cross-owner = managed copy+delete, spec §9)
    }

    fn statfs(&self) -> Result<StatFs, StoreError> {
        Err(StoreError::Unimplemented) // TODO: Statfs rpc / aggregate catalog observations
    }
}
