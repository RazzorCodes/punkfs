use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use tonic::transport::Server;
use tonic::{Request, Response, Status};

use punkfs::punk_fs::punk_fs_server::{PunkFs, PunkFsServer};
use punkfs::punk_fs::{DirEntry, FileKind, ListDirectoryRequest, ListDirectoryResponse};

struct PunkFsService {
    root: PathBuf,
}

impl PunkFsService {
    /// Turn a client-supplied path into a real path under `root`,
    /// rejecting anything that could escape it (`..`, absolute prefixes).
    fn resolve(&self, rel: &str) -> Result<PathBuf, Status> {
        let rel = Path::new(rel.trim_start_matches('/'));
        for c in rel.components() {
            if !matches!(c, Component::Normal(_)) {
                return Err(Status::invalid_argument("invalid path"));
            }
        }
        Ok(self.root.join(rel))
    }
}

#[tonic::async_trait]
impl PunkFs for PunkFsService {
    async fn list_directory(
        &self,
        request: Request<ListDirectoryRequest>,
    ) -> Result<Response<ListDirectoryResponse>, Status> {
        let dir = self.resolve(&request.get_ref().path)?;

        let mut rd = tokio::fs::read_dir(&dir).await.map_err(io_status)?;
        let mut entries = Vec::new();

        while let Some(entry) = rd.next_entry().await.map_err(io_status)? {
            let file_type = entry.file_type().await.map_err(io_status)?;
            let meta = entry.metadata().await.map_err(io_status)?;

            let kind = if file_type.is_dir() {
                FileKind::Directory
            } else if file_type.is_symlink() {
                FileKind::Symlink
            } else {
                FileKind::File
            };

            let mtime_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind: kind as i32,
                size: meta.len(),
                mtime_unix,
            });
        }

        Ok(Response::new(ListDirectoryResponse { entries }))
    }
}

fn io_status(e: std::io::Error) -> Status {
    match e.kind() {
        std::io::ErrorKind::NotFound => Status::not_found(e.to_string()),
        std::io::ErrorKind::PermissionDenied => Status::permission_denied(e.to_string()),
        _ => Status::internal(e.to_string()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // usage: punkfs-server [root_dir] [listen_addr]
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let addr = args
        .next()
        .unwrap_or_else(|| "[::1]:10000".into())
        .parse()?;

    let root = tokio::fs::canonicalize(&root).await?;
    println!("serving {} on {addr}", root.display());

    Server::builder()
        .add_service(PunkFsServer::new(PunkFsService { root }))
        .serve(addr)
        .await?;

    Ok(())
}
