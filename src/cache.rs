use std::collections::HashSet;
use std::sync::Mutex;

use tokio::runtime::Runtime;
use tonic::transport::{Channel, Endpoint};

use crate::db::fstree::FsTree;
use crate::punk_fs::ListDirectoryRequest;
use crate::punk_fs::punk_fs_client::PunkFsClient;

pub struct Cache {
    fstree: Mutex<FsTree>,
    listed: Mutex<HashSet<String>>, // directories already fetched from the server
    rt: Runtime,
    client: PunkFsClient<Channel>,
}

impl Cache {
    pub fn new(server_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let rt = Runtime::new()?;
        // connect_lazy doesn't fail if the server is down; the first RPC connects.
        // It needs a runtime context, hence enter().
        let channel = {
            let _guard = rt.enter();
            Endpoint::from_shared(server_url.to_owned())?.connect_lazy()
        };

        let fstree = FsTree { entries: vec![] };
        let cache = Cache {
            fstree: Mutex::new(fstree),
            listed: Mutex::new(HashSet::new()),
            rt,
            client: PunkFsClient::new(channel),
        };
        cache.upsert_entry("/", 1); // root
        Ok(cache)
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

    pub fn upsert_entry(&self, path: &str, inode: u64) -> Option<(String, u64)> {
        let old = self.fstree.lock().unwrap().upsert_entry(path, inode)?;
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
            tree.upsert_entry(&full, 0); // 0 = allocate, or reuse if the path exists
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
