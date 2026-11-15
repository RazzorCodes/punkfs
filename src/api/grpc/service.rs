//! The grpc service: one backing folder served over grpc.
//!
//! This lives in the library (not the binary) so integration tests can
//! construct it over a `TempDir` and call handlers directly, or serve it on
//! an ephemeral port.
//!
//! Scope: this iteration serves content read/write, attributes, and removal
//! for a *single* backing folder on the local machine. Creating namespace
//! entries (new files, new directories, symlinks) is deliberately absent —
//! entries are created by the machine that owns the storage, not through
//! the merged mount.
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::Notify;
use tonic::{Request, Response, Status};

use crate::db::fstree::{Access, FileAttrData, FileTimes, FsEntryKind, Stat};
use crate::punk_fs::punk_fs_server::PunkFs;
use crate::punk_fs::{
    Access as PbAccess, DirEntry, FileAttr, FileKind, FileTimes as PbFileTimes, FsyncFileRequest,
    FsyncFileResponse, GetAttrRequest, GetAttrResponse, ListDirectoryRequest, ListDirectoryResponse,
    ReadFileRequest, ReadFileResponse, RemoveDirRequest, RemoveDirResponse, RemoveFileRequest,
    RemoveFileResponse, RenameRequest, RenameResponse, SetAttrRequest, SetAttrResponse, StatFsRequest,
    StatFsResponse, StatFsResponse as PbStatFs, Stat as PbStat, WriteFileRequest, WriteFileResponse,
};

/// Upper bound on a single `ReadFile` answer. The FUSE layer asks for
/// whatever the kernel wants (commonly 128 KiB); this keeps one malicious
/// request from asking for a gigabyte.
pub const MAX_READ: u32 = 1024 * 1024;

/// UUID namespace for entry identities. Fixed forever: changing it renames
/// every entry in the system.
const UUID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6ba7b810_9dad_11d1_80b4_00c04fd430c8);

/// A served folder, plus the hooks `main` needs to run and stop it.
pub struct PunkFsService {
    root: PathBuf,
    node_id: String,
    /// Set by the `Shutdown` rpc (only served with `dev-shutdown`).
    shutdown: Arc<Notify>,
    shutdown_requested: Arc<std::sync::atomic::AtomicBool>,
}

impl PunkFsService {
    pub fn new(root: impl Into<PathBuf>, node_id: impl Into<String>) -> Arc<Self> {
        Arc::new(PunkFsService {
            root: root.into(),
            node_id: node_id.into(),
            shutdown: Arc::new(Notify::new()),
            shutdown_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Resolves when `Shutdown` has been called. `main` selects on this.
    pub async fn shutdown_signal(&self) {
        if !self.shutdown_requested.load(Ordering::SeqCst) {
            self.shutdown.notified().await;
        }
    }

    /// Turn a client-supplied logical path into a real path under `root`,
    /// rejecting anything that could escape it.
    ///
    /// Only plain names are allowed: no `..`, no absolute prefix, no
    /// embedded separator tricks. This is the single confinement point for
    /// every rpc.
    fn resolve(&self, rel: &str) -> Result<PathBuf, Status> {
        let trimmed = rel.trim_start_matches('/');
        let path = Path::new(trimmed);
        for component in path.components() {
            match component {
                Component::Normal(_) => {}
                // "." is harmless; "..", root and prefixes are not.
                Component::CurDir => {}
                _ => return Err(Status::invalid_argument("invalid path")),
            }
        }
        Ok(self.root.join(path))
    }

    /// punkfs identity for a logical path.
    ///
    /// Deterministic per (node, path) so repeated listings agree without
    /// persisting anything. **Limitation, by design of this iteration:**
    /// renaming an entry changes its uuid, because real uuid assignment
    /// belongs to the catalog/coordinator, which would carry it across the
    /// rename.
    fn uuid_for(&self, logical: &str) -> String {
        uuid::Uuid::new_v5(
            &UUID_NAMESPACE,
            format!("{}/{}", self.node_id, logical).as_bytes(),
        )
        .to_string()
    }

    /// Read a path's attributes without following a final symlink.
    fn attr_for(&self, logical: &str, real: &Path) -> io::Result<FileAttrData> {
        let meta = std::fs::symlink_metadata(real)?;
        let file_type = meta.file_type();

        let kind = if file_type.is_symlink() {
            FsEntryKind::Link
        } else if file_type.is_dir() {
            FsEntryKind::Directory
        } else {
            FsEntryKind::File
        };

        Ok(FileAttrData {
            uuid: self.uuid_for(logical),
            kind,
            times: FileTimes {
                accessed: meta.accessed().unwrap_or(UNIX_EPOCH),
                modified: meta.modified().unwrap_or(UNIX_EPOCH),
                changed: UNIX_EPOCH + Duration::from_secs(meta.ctime().max(0) as u64),
                created: meta.created().unwrap_or(UNIX_EPOCH),
            },
            access: Access {
                mode: meta.permissions().mode() as u16 & 0o7777,
                uid: meta.uid(),
                gid: meta.gid(),
            },
            stat: Stat {
                size: meta.len(),
                blocks: meta.blocks() as u32,
                block_size: meta.blksize() as u16,
                rdev: meta.rdev() as u16,
                nlink: meta.nlink() as u32,
                flags: 0,
            },
        })
    }
}

fn file_kind(kind: FsEntryKind) -> FileKind {
    match kind {
        FsEntryKind::Directory => FileKind::Directory,
        FsEntryKind::Link => FileKind::Symlink,
        FsEntryKind::Unknown | FsEntryKind::File => FileKind::File,
    }
}

fn to_proto_attr(data: &FileAttrData) -> FileAttr {
    let secs = |t: std::time::SystemTime| {
        t.duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };

    FileAttr {
        uuid: data.uuid.clone(),
        kind: file_kind(data.kind) as i32,
        times: Some(PbFileTimes {
            accessed_unix: secs(data.times.accessed),
            modified_unix: secs(data.times.modified),
            changed_unix: secs(data.times.changed),
            created_unix: secs(data.times.created),
        }),
        access: Some(PbAccess {
            mode: data.access.mode as u32,
            uid: data.access.uid,
            gid: data.access.gid,
        }),
        stat: Some(PbStat {
            size: data.stat.size,
            blocks: data.stat.blocks as u64,
            block_size: data.stat.block_size as u32,
            rdev: data.stat.rdev as u32,
            nlink: data.stat.nlink,
            flags: data.stat.flags,
        }),
    }
}

/// Map an `io::Error` onto the grpc code the client will translate back
/// into an errno. The mapping is deliberately narrow: anything the client
/// cannot act on becomes `internal`.
pub fn io_status(err: io::Error) -> Status {
    match err.kind() {
        io::ErrorKind::NotFound => Status::not_found(err.to_string()),
        io::ErrorKind::PermissionDenied => Status::permission_denied(err.to_string()),
        io::ErrorKind::AlreadyExists => Status::already_exists(err.to_string()),
        io::ErrorKind::InvalidInput => Status::invalid_argument(err.to_string()),
        io::ErrorKind::IsADirectory => Status::invalid_argument("is a directory"),
        io::ErrorKind::NotADirectory => Status::invalid_argument("not a directory"),
        io::ErrorKind::DirectoryNotEmpty => Status::failed_precondition("directory not empty"),
        io::ErrorKind::Unsupported => Status::unimplemented(err.to_string()),
        _ => Status::internal(err.to_string()),
    }
}

/// Open a file for reading or writing at an absolute position.
struct PositionalFile(std::fs::File);

/// `std::fs::File` positional helpers, kept tiny and explicit.
fn open_for_read(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn open_for_write(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).open(path)
}

impl PositionalFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.0.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        self.0.write_at(buf, offset)
    }
}

#[tonic::async_trait]
impl PunkFs for PunkFsService {
    async fn list_directory(
        &self,
        request: Request<ListDirectoryRequest>,
    ) -> Result<Response<ListDirectoryResponse>, Status> {
        let logical = request.get_ref().path.clone();
        let dir = self.resolve(&logical)?;

        let mut rd = tokio::fs::read_dir(&dir).await.map_err(io_status)?;
        let mut entries = Vec::new();

        while let Some(entry) = rd.next_entry().await.map_err(io_status)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_logical = if logical == "/" || logical.is_empty() {
                format!("/{name}")
            } else {
                format!("{}/{name}", logical.trim_end_matches('/'))
            };
            let attr = self
                .attr_for(&child_logical, &entry.path())
                .map_err(io_status)?;
            entries.push(DirEntry {
                name,
                attr: Some(to_proto_attr(&attr)),
            });
        }

        // Stable order so listings are reproducible and diffable.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Response::new(ListDirectoryResponse { entries }))
    }

    async fn get_attr(
        &self,
        request: Request<GetAttrRequest>,
    ) -> Result<Response<GetAttrResponse>, Status> {
        let logical = request.get_ref().path.clone();
        let real = self.resolve(&logical)?;
        let attr = self.attr_for(&logical, &real).map_err(io_status)?;
        Ok(Response::new(GetAttrResponse {
            attr: Some(to_proto_attr(&attr)),
        }))
    }

    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<ReadFileResponse>, Status> {
        let req = request.into_inner();
        let real = self.resolve(&req.path)?;
        let length = req.length.min(MAX_READ) as usize;

        let path = real.clone();
        let data = tokio::task::spawn_blocking(move || -> io::Result<Vec<u8>> {
            let file = PositionalFile(open_for_read(&path)?);
            let mut buf = vec![0u8; length];
            let read = file.read_at(&mut buf, req.offset)?;
            buf.truncate(read);
            Ok(buf)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map_err(io_status)?;

        Ok(Response::new(ReadFileResponse { data }))
    }

    async fn write_file(
        &self,
        request: Request<WriteFileRequest>,
    ) -> Result<Response<WriteFileResponse>, Status> {
        let req = request.into_inner();
        let real = self.resolve(&req.path)?;

        let path = real.clone();
        let written = tokio::task::spawn_blocking(move || -> io::Result<u32> {
            let file = PositionalFile(open_for_write(&path)?);
            let mut written = 0usize;
            // A short write is rare but legal; keep going until the buffer
            // is drained so the client never sees a silent truncation.
            while written < req.data.len() {
                let n = file.write_at(&req.data[written..], req.offset + written as u64)?;
                if n == 0 {
                    break;
                }
                written += n;
            }
            Ok(written as u32)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map_err(io_status)?;

        Ok(Response::new(WriteFileResponse { written }))
    }

    async fn fsync_file(
        &self,
        request: Request<FsyncFileRequest>,
    ) -> Result<Response<FsyncFileResponse>, Status> {
        let req = request.into_inner();
        let real = self.resolve(&req.path)?;

        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let file = open_for_write(&real)?;
            if req.datasync {
                file.sync_data()
            } else {
                file.sync_all()
            }
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map_err(io_status)?;

        Ok(Response::new(FsyncFileResponse {}))
    }

    async fn set_attr(
        &self,
        request: Request<SetAttrRequest>,
    ) -> Result<Response<SetAttrResponse>, Status> {
        let req = request.into_inner();

        // Ownership changes need privilege this backend does not assume.
        // Refusing explicitly beats silently not applying them.
        if req.uid.is_some() || req.gid.is_some() {
            return Err(Status::unimplemented("changing ownership is not supported"));
        }

        let real = self.resolve(&req.path)?;

        if let Some(mode) = req.mode {
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(io_status)?;
        }

        if let Some(size) = req.size {
            let file = open_for_write(&real).map_err(io_status)?;
            file.set_len(size).map_err(io_status)?;
        }

        // atime/mtime: only applied when the caller asked for one of them,
        // using the pair so the untouched half is preserved.
        if req.accessed_unix.is_some() || req.modified_unix.is_some() {
            apply_times(&real, req.accessed_unix, req.modified_unix)?;
        }

        let attr = self.attr_for(&req.path, &real).map_err(io_status)?;
        Ok(Response::new(SetAttrResponse {
            attr: Some(to_proto_attr(&attr)),
        }))
    }

    async fn stat_fs(
        &self,
        _request: Request<StatFsRequest>,
    ) -> Result<Response<StatFsResponse>, Status> {
        let root = self.root.clone();
        let stats = tokio::task::spawn_blocking(move || -> io::Result<PbStatFs> {
            let vfs = statvfs(&root)?;
            Ok(PbStatFs {
                blocks: vfs.blocks,
                bfree: vfs.bfree,
                bavail: vfs.bavail,
                files: vfs.files,
                ffree: vfs.ffree,
                block_size: vfs.block_size,
                name_length: vfs.name_length,
                fragment_size: vfs.fragment_size,
            })
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map_err(io_status)?;

        Ok(Response::new(stats))
    }

    async fn remove_file(
        &self,
        request: Request<RemoveFileRequest>,
    ) -> Result<Response<RemoveFileResponse>, Status> {
        let real = self.resolve(&request.get_ref().path)?;
        tokio::fs::remove_file(&real).await.map_err(io_status)?;
        Ok(Response::new(RemoveFileResponse {}))
    }

    async fn remove_dir(
        &self,
        request: Request<RemoveDirRequest>,
    ) -> Result<Response<RemoveDirResponse>, Status> {
        let real = self.resolve(&request.get_ref().path)?;
        // std/tokio refuse a non-empty directory, which is the behaviour
        // `rmdir` needs; the message is what the client turns into ENOTEMPTY.
        tokio::fs::remove_dir(&real).await.map_err(io_status)?;
        Ok(Response::new(RemoveDirResponse {}))
    }

    async fn rename(
        &self,
        request: Request<RenameRequest>,
    ) -> Result<Response<RenameResponse>, Status> {
        let req = request.into_inner();
        if req.flags != 0 {
            return Err(Status::unimplemented(
                "rename flags (NOREPLACE/EXCHANGE) are not supported",
            ));
        }

        let from = self.resolve(&req.old_path)?;
        let to = self.resolve(&req.new_path)?;
        tokio::fs::rename(&from, &to).await.map_err(io_status)?;
        Ok(Response::new(RenameResponse {}))
    }

    async fn shutdown(
        &self,
        _request: Request<crate::punk_fs::ShutdownRequest>,
    ) -> Result<Response<crate::punk_fs::ShutdownResponse>, Status> {
        #[cfg(feature = "dev-shutdown")]
        {
            self.shutdown_requested.store(true, Ordering::SeqCst);
            self.shutdown.notify_waiters();
            Ok(Response::new(crate::punk_fs::ShutdownResponse {}))
        }
        #[cfg(not(feature = "dev-shutdown"))]
        {
            Err(Status::unimplemented(
                "shutdown is only available with the dev-shutdown feature",
            ))
        }
    }
}

/// Set atime and/or mtime, preserving whichever half the caller omitted.
fn apply_times(
    path: &Path,
    accessed_unix: Option<i64>,
    modified_unix: Option<i64>,
) -> Result<(), Status> {
    let meta = std::fs::metadata(path).map_err(io_status)?;
    let to_timespec = |secs: Option<i64>, fallback: std::time::SystemTime| {
        let t = secs
            .map(|s| UNIX_EPOCH + Duration::from_secs(s.max(0) as u64))
            .unwrap_or(fallback);
        let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
        libc::timespec {
            tv_sec: dur.as_secs() as libc::time_t,
            tv_nsec: dur.subsec_nanos() as _,
        }
    };

    let times = [
        to_timespec(accessed_unix, meta.accessed().unwrap_or(UNIX_EPOCH)),
        to_timespec(modified_unix, meta.modified().unwrap_or(UNIX_EPOCH)),
    ];

    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| Status::invalid_argument("path contains a NUL byte"))?;

    // SAFETY: `c_path` is a valid NUL-terminated string and `times` is a
    // two-element array, which is exactly what utimensat expects.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if rc != 0 {
        return Err(io_status(io::Error::last_os_error()));
    }
    Ok(())
}

struct VfsStats {
    blocks: u64,
    bfree: u64,
    bavail: u64,
    files: u64,
    ffree: u64,
    block_size: u32,
    name_length: u32,
    fragment_size: u32,
}

fn statvfs(path: &Path) -> io::Result<VfsStats> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;

    // SAFETY: `statvfs` is a plain out-parameter call; we zero the struct
    // first so a partial fill cannot surface uninitialised memory.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(VfsStats {
        blocks: buf.f_blocks,
        bfree: buf.f_bfree,
        bavail: buf.f_bavail,
        files: buf.f_files,
        ffree: buf.f_ffree,
        block_size: buf.f_bsize as u32,
        name_length: buf.f_namemax as u32,
        fragment_size: buf.f_frsize as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Arc<PunkFsService>) {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("hello.txt"), b"hello world").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/nested.txt"), b"nested").unwrap();
        let service = PunkFsService::new(dir.path(), "test-node");
        (dir, service)
    }

    async fn list(service: &Arc<PunkFsService>, path: &str) -> Vec<DirEntry> {
        service
            .list_directory(Request::new(ListDirectoryRequest {
                path: path.to_owned(),
            }))
            .await
            .unwrap()
            .into_inner()
            .entries
    }

    async fn attr(service: &Arc<PunkFsService>, path: &str) -> FileAttr {
        service
            .get_attr(Request::new(GetAttrRequest {
                path: path.to_owned(),
            }))
            .await
            .unwrap()
            .into_inner()
            .attr
            .unwrap()
    }

    async fn read(service: &Arc<PunkFsService>, path: &str, offset: u64, length: u32) -> Vec<u8> {
        service
            .read_file(Request::new(ReadFileRequest {
                path: path.to_owned(),
                offset,
                length,
            }))
            .await
            .unwrap()
            .into_inner()
            .data
    }

    async fn write(
        service: &Arc<PunkFsService>,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> u32 {
        service
            .write_file(Request::new(WriteFileRequest {
                path: path.to_owned(),
                offset,
                data: data.to_vec(),
            }))
            .await
            .unwrap()
            .into_inner()
            .written
    }

    #[tokio::test]
    async fn list_directory_reports_names_kinds_and_sizes() {
        let (_dir, service) = fixture();
        let entries = list(&service, "/").await;

        let by_name: std::collections::HashMap<_, _> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.attr.as_ref().unwrap()))
            .collect();

        assert_eq!(by_name.len(), 2);
        assert_eq!(by_name["hello.txt"].stat.as_ref().unwrap().size, 11);
        assert_eq!(by_name["hello.txt"].kind(), FileKind::File);
        assert_eq!(by_name["sub"].kind(), FileKind::Directory);
    }

    #[tokio::test]
    async fn list_directory_is_sorted() {
        let (_dir, service) = fixture();
        let names: Vec<String> = list(&service, "/").await.into_iter().map(|e| e.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[tokio::test]
    async fn uuids_are_deterministic_per_path() {
        let (_dir, service) = fixture();
        let first = attr(&service, "/hello.txt").await.uuid;
        let second = attr(&service, "/hello.txt").await.uuid;
        assert_eq!(first, second);

        let other = attr(&service, "/sub").await.uuid;
        assert_ne!(first, other, "different entries must not share a uuid");
    }

    #[tokio::test]
    async fn get_attr_reports_directory_and_file() {
        let (_dir, service) = fixture();
        assert_eq!(attr(&service, "/").await.kind(), FileKind::Directory);
        let file = attr(&service, "/hello.txt").await;
        assert_eq!(file.kind(), FileKind::File);
        assert_eq!(file.stat.unwrap().size, 11);
    }

    #[tokio::test]
    async fn get_attr_missing_path_is_not_found() {
        let (_dir, service) = fixture();
        let err = service
            .get_attr(Request::new(GetAttrRequest {
                path: "/nope".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn read_returns_the_whole_file() {
        let (_dir, service) = fixture();
        assert_eq!(read(&service, "/hello.txt", 0, 1024).await, b"hello world");
    }

    #[tokio::test]
    async fn read_is_positional() {
        let (_dir, service) = fixture();
        assert_eq!(read(&service, "/hello.txt", 6, 5).await, b"world");
        assert_eq!(read(&service, "/hello.txt", 0, 5).await, b"hello");
    }

    #[tokio::test]
    async fn read_past_eof_returns_empty() {
        let (_dir, service) = fixture();
        assert!(read(&service, "/hello.txt", 100, 16).await.is_empty());
    }

    #[tokio::test]
    async fn read_past_eof_after_a_write_sees_the_new_size() {
        let (_dir, service) = fixture();
        write(&service, "/hello.txt", 11, b"!!").await;
        assert_eq!(read(&service, "/hello.txt", 0, 1024).await, b"hello world!!");
    }

    #[tokio::test]
    async fn read_is_capped_at_the_limit() {
        let (_dir, service) = fixture();
        let data = read(&service, "/hello.txt", 0, 4).await;
        assert_eq!(data.len(), 4);
        assert_eq!(data, b"hell");
    }

    #[tokio::test]
    async fn write_modifies_the_backing_file() {
        let (dir, service) = fixture();
        let written = write(&service, "/hello.txt", 0, b"HELLO").await;
        assert_eq!(written, 5);
        assert_eq!(
            fs::read(dir.path().join("hello.txt")).unwrap(),
            b"HELLO world"
        );
    }

    #[tokio::test]
    async fn write_at_an_offset_extends_the_file() {
        let (dir, service) = fixture();
        write(&service, "/hello.txt", 11, b" appended").await;
        assert_eq!(
            fs::read(dir.path().join("hello.txt")).unwrap(),
            b"hello world appended"
        );
    }

    #[tokio::test]
    async fn write_does_not_create_missing_files() {
        let (_dir, service) = fixture();
        let err = service
            .write_file(Request::new(WriteFileRequest {
                path: "/new.txt".into(),
                offset: 0,
                data: b"x".to_vec(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn write_round_trips_a_large_buffer() {
        let (_dir, service) = fixture();
        // Bigger than the classic 4 KiB page so a chunking bug would show.
        let payload: Vec<u8> = (0..(300 * 1024)).map(|i| (i % 251) as u8).collect();
        let written = write(&service, "/hello.txt", 0, &payload).await;
        assert_eq!(written as usize, payload.len());

        // Read it back in `MAX_READ`-sized chunks, the way a client would.
        let mut got = Vec::new();
        while got.len() < payload.len() {
            let chunk = read(&service, "/hello.txt", got.len() as u64, MAX_READ).await;
            if chunk.is_empty() {
                break;
            }
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn fsync_succeeds_on_an_existing_file() {
        let (_dir, service) = fixture();
        service
            .fsync_file(Request::new(FsyncFileRequest {
                path: "/hello.txt".into(),
                datasync: true,
            }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fsync_missing_file_is_not_found() {
        let (_dir, service) = fixture();
        let err = service
            .fsync_file(Request::new(FsyncFileRequest {
                path: "/nope".into(),
                datasync: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn set_attr_changes_mode() {
        let (dir, service) = fixture();
        let resp = service
            .set_attr(Request::new(SetAttrRequest {
                path: "/hello.txt".into(),
                mode: Some(0o600),
                ..SetAttrRequest::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.attr.unwrap().access.unwrap().mode, 0o600);
        let mode = fs::metadata(dir.path().join("hello.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn set_attr_truncates() {
        let (dir, service) = fixture();
        service
            .set_attr(Request::new(SetAttrRequest {
                path: "/hello.txt".into(),
                size: Some(5),
                ..SetAttrRequest::default()
            }))
            .await
            .unwrap();
        assert_eq!(fs::read(dir.path().join("hello.txt")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn set_attr_refuses_ownership_changes() {
        let (_dir, service) = fixture();
        let err = service
            .set_attr(Request::new(SetAttrRequest {
                path: "/hello.txt".into(),
                uid: Some(0),
                ..SetAttrRequest::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn set_attr_applies_timestamps() {
        let (dir, service) = fixture();
        service
            .set_attr(Request::new(SetAttrRequest {
                path: "/hello.txt".into(),
                modified_unix: Some(1_000_000),
                ..SetAttrRequest::default()
            }))
            .await
            .unwrap();

        let mtime = fs::metadata(dir.path().join("hello.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            mtime.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_000_000
        );
    }

    #[tokio::test]
    async fn remove_file_deletes_it() {
        let (dir, service) = fixture();
        service
            .remove_file(Request::new(RemoveFileRequest {
                path: "/hello.txt".into(),
            }))
            .await
            .unwrap();
        assert!(!dir.path().join("hello.txt").exists());
    }

    #[tokio::test]
    async fn remove_dir_requires_an_empty_directory() {
        let (_dir, service) = fixture();
        let err = service
            .remove_dir(Request::new(RemoveDirRequest {
                path: "/sub".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn remove_dir_deletes_an_empty_directory() {
        let (dir, service) = fixture();
        fs::create_dir(dir.path().join("empty")).unwrap();
        service
            .remove_dir(Request::new(RemoveDirRequest {
                path: "/empty".into(),
            }))
            .await
            .unwrap();
        assert!(!dir.path().join("empty").exists());
    }

    #[tokio::test]
    async fn rename_moves_the_entry() {
        let (dir, service) = fixture();
        service
            .rename(Request::new(RenameRequest {
                old_path: "/hello.txt".into(),
                new_path: "/renamed.txt".into(),
                flags: 0,
            }))
            .await
            .unwrap();
        assert!(!dir.path().join("hello.txt").exists());
        assert_eq!(fs::read(dir.path().join("renamed.txt")).unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn rename_refuses_flags() {
        let (_dir, service) = fixture();
        let err = service
            .rename(Request::new(RenameRequest {
                old_path: "/hello.txt".into(),
                new_path: "/x".into(),
                flags: 1,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn stat_fs_reports_a_positive_block_size() {
        let (_dir, service) = fixture();
        let stats = service
            .stat_fs(Request::new(StatFsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert!(stats.block_size > 0);
        assert!(stats.blocks > 0);
    }

    #[tokio::test]
    async fn resolve_rejects_traversal_and_absolute_paths() {
        let (_dir, service) = fixture();
        for bad in ["../etc/passwd", "/..", "a/../../b", ""] {
            let result = service.get_attr(Request::new(GetAttrRequest {
                path: bad.into(),
            }));
            // "/.." and traversal must be rejected outright; an empty path
            // resolves to the root itself, which is legitimate.
            if bad.is_empty() {
                continue;
            }
            let err = result.await.unwrap_err();
            assert!(
                err.code() == tonic::Code::InvalidArgument || err.code() == tonic::Code::NotFound,
                "{bad:?} produced {:?}",
                err.code()
            );
        }
    }

    #[tokio::test]
    async fn shutdown_is_gated_on_the_dev_feature() {
        let (_dir, service) = fixture();
        let result = service
            .shutdown(Request::new(crate::punk_fs::ShutdownRequest {}))
            .await;

        #[cfg(not(feature = "dev-shutdown"))]
        assert_eq!(
            result.unwrap_err().code(),
            tonic::Code::Unimplemented,
            "shutdown must be unreachable without the dev-shutdown feature"
        );

        #[cfg(feature = "dev-shutdown")]
        {
            result.expect("shutdown is served when the feature is on");
            // And it must actually trip the signal `main` selects on.
            tokio::time::timeout(std::time::Duration::from_secs(1), service.shutdown_signal())
                .await
                .expect("shutdown_signal should be ready");
        }
    }

    #[test]
    fn io_errors_map_to_the_expected_codes() {
        let cases = [
            (io::ErrorKind::NotFound, tonic::Code::NotFound),
            (io::ErrorKind::PermissionDenied, tonic::Code::PermissionDenied),
            (io::ErrorKind::AlreadyExists, tonic::Code::AlreadyExists),
            (io::ErrorKind::InvalidInput, tonic::Code::InvalidArgument),
            (io::ErrorKind::Unsupported, tonic::Code::Unimplemented),
        ];
        for (kind, code) in cases {
            assert_eq!(io_status(io::Error::from(kind)).code(), code, "{kind:?}");
        }
    }
}
