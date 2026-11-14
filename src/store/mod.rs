use std::collections::HashSet;
use std::sync::Mutex;

use tokio::runtime::Runtime;
use tonic::transport::{Channel, Endpoint};

use crate::db::fstree::{FsEntry, FsEntryKind, FsMetadata, FsNode, FsTree};
use crate::punk_fs::{FileKind, ListDirectoryRequest};
use crate::punk_fs::punk_fs_client::PunkFsClient;

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
            node: node,
        };
        store.upsert_entry("/", 1, FsEntryKind::Directory, None); // root
        Ok(store)
    }

    pub fn get_inode(&self, path: &str) -> Option<u64> {
        self.fstree.lock().unwrap().get_inode(path)
    }

    pub fn get_path(&self, inode: u64) -> Option<String> {
        self.fstree
            .lock()
            .unwrap()
            .get_path(inode)
            .map(str::to_owned)
    }

    /// The whole entry for an inode (kind, metadata, path), for callers
    /// that need more than the path.
    pub fn get_entry(&self, inode: u64) -> Option<FsEntry> {
        let tree = self.fstree.lock().unwrap();
        tree.entries
            .iter()
            .find(|e| e.inode == inode)
            .cloned()
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
    pub fn ensure_listed(&self, dir: &str) -> Result<(), tonic::Status> {
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

    /// Children as (inode, *name*). On a server error we serve whatever is cached.
    pub fn get_children(&self, path: &str) -> Option<Vec<(u64, String)>> {
        let _ = self.ensure_listed(path); // offline => stale or empty listing, not an error

        let tree = self.fstree.lock().unwrap();
        Some(
            tree.get_children_of(path)
                .into_iter()
                .map(|e| {
                    let name = e.path.rsplit('/').next().unwrap_or("").to_owned();
                    (e.inode, name)
                })
                .collect(),
        )
    }
}
