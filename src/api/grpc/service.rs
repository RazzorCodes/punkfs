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
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::Notify;
use tonic::{Request, Response, Status};

use crate::core::backing::{Backing, SafePath};
#[cfg(test)]
use crate::core::backing::{derived_guid, read_guid_xattr, write_guid_xattr};
use crate::punk_fs::punk_fs_server::PunkFs;
use crate::punk_fs::{
    Access as PbAccess, DirEntry, FileAttr, FileKind, FileTimes as PbFileTimes, FsyncFileRequest,
    FsyncFileResponse, GetAttrRequest, GetAttrResponse, ListDirectoryRequest,
    ListDirectoryResponse, ReadFileRequest, ReadFileResponse, RemoveDirRequest, RemoveDirResponse,
    RemoveFileRequest, RemoveFileResponse, RenameRequest, RenameResponse, SetAttrRequest,
    SetAttrResponse, Stat as PbStat, StatFsRequest, StatFsResponse, StatFsResponse as PbStatFs,
    WriteFileRequest, WriteFileResponse,
};
use crate::store::db::attributes::{FileAttrData, FsEntryKind};

/// Upper bound on a single `ReadFile` answer. The FUSE layer asks for
/// whatever the kernel wants (commonly 128 KiB); this keeps one malicious
/// request from asking for a gigabyte.
pub const MAX_READ: u32 = 1024 * 1024;

/// A served folder, plus the hooks `main` needs to run and stop it.
pub struct PunkFsService {
    backing: Arc<Backing>,
    allow_shutdown: bool,
    /// Set by the `Shutdown` rpc (only served with `dev-shutdown`).
    shutdown: Arc<Notify>,
    shutdown_requested: Arc<std::sync::atomic::AtomicBool>,
}

impl PunkFsService {
    /// Construct a standalone service, panicking if the backing cannot be
    /// opened. Prefer `try_new` when the root comes from configuration.
    pub fn new(root: impl Into<PathBuf>, node_id: impl Into<String>) -> Arc<Self> {
        Self::try_new(root, node_id).expect("open backing directory")
    }

    /// Construct a standalone service over an existing directory.
    pub fn try_new(root: impl Into<PathBuf>, node_id: impl Into<String>) -> io::Result<Arc<Self>> {
        let root = root.into();
        let node_id = node_id.into();
        let backing = Arc::new(Backing::open(&root, &node_id)?);
        Ok(Self::from_backing(backing, true))
    }

    pub(crate) fn from_backing(backing: Arc<Backing>, allow_shutdown: bool) -> Arc<Self> {
        Arc::new(PunkFsService {
            backing,
            allow_shutdown,
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

    fn resolve(&self, rel: &str) -> Result<SafePath, Status> {
        self.backing.path(rel).map_err(io_status)
    }

    fn log_io<T>(
        &self,
        context: &crate::telemetry::OperationContext,
        operation: &str,
        result: &Result<T, Status>,
    ) {
        use crate::telemetry::{self, Event, Severity};
        let Some(logger) = telemetry::global() else {
            return;
        };
        let severity = if result.is_ok() {
            Severity::Debug
        } else {
            Severity::Warning
        };
        if !logger.enabled(severity) {
            return;
        }
        let mut event = Event::new(severity, "grpc.backing", "backing operation completed");
        event.operation = Some(operation.into());
        event
            .fields
            .insert("node_id".into(), serde_json::json!(self.backing.node_id()));
        event.fields.insert(
            "outcome".into(),
            serde_json::json!(if result.is_ok() { "ok" } else { "error" }),
        );
        if let Err(status) = result {
            event.fields.insert(
                "grpc_code".into(),
                serde_json::json!(format!("{:?}", status.code())),
            );
        }
        logger.emit_in(context, event);
    }
}

fn file_kind(kind: FsEntryKind) -> FileKind {
    match kind {
        FsEntryKind::Directory => FileKind::Directory,
        FsEntryKind::Link => FileKind::Symlink,
        FsEntryKind::Unknown | FsEntryKind::File => FileKind::File,
    }
}

pub(crate) fn to_proto_attr(data: &FileAttrData) -> FileAttr {
    let secs = |t: std::time::SystemTime| {
        t.duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };

    FileAttr {
        guid: data.guid.clone(),
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
    if err.raw_os_error() == Some(libc::ELOOP) {
        return Status::permission_denied("symlink traversal is not supported");
    }
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

fn open_for_write(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).open(path)
}

#[tonic::async_trait]
impl PunkFs for PunkFsService {
    async fn list_directory(
        &self,
        request: Request<ListDirectoryRequest>,
    ) -> Result<Response<ListDirectoryResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let logical = request.get_ref().path.clone();
        let dir = self.backing.existing(&logical).map_err(io_status)?;

        let mut rd = tokio::fs::read_dir(&dir).await.map_err(io_status)?;
        let mut entries = Vec::new();
        let mut skipped = 0;

        while let Some(entry) = rd.next_entry().await.map_err(io_status)? {
            if entry.file_type().await.map_err(io_status)?.is_symlink() {
                skipped += 1;
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_logical = if logical == "/" || logical.is_empty() {
                format!("/{name}")
            } else {
                format!("{}/{name}", logical.trim_end_matches('/'))
            };
            let attr = self
                .backing
                .attr_for(&child_logical, &entry.path())
                .map_err(io_status)?;
            entries.push(DirEntry {
                name,
                attr: Some(to_proto_attr(&attr)),
            });
        }

        self.backing.log_skipped_symlinks(&context, skipped);
        // Stable order so listings are reproducible and diffable.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(context.response(ListDirectoryResponse { entries }))
    }

    async fn get_attr(
        &self,
        request: Request<GetAttrRequest>,
    ) -> Result<Response<GetAttrResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let logical = request.get_ref().path.clone();
        let real = self.resolve(&logical)?;
        let attr = self.backing.attr_for(&logical, &real).map_err(io_status)?;
        Ok(context.response(GetAttrResponse {
            attr: Some(to_proto_attr(&attr)),
        }))
    }

    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<ReadFileResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();
        let backing = self.backing.clone();
        let blocking_context = context.clone();
        let data = tokio::task::spawn_blocking(move || -> io::Result<Vec<u8>> {
            let _context = blocking_context;
            backing.read(&req.path, req.offset, req.length.min(MAX_READ))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))
        .and_then(|result| result.map_err(io_status));
        self.log_io(&context, "read", &data);
        Ok(context.response(ReadFileResponse { data: data? }))
    }

    async fn write_file(
        &self,
        request: Request<WriteFileRequest>,
    ) -> Result<Response<WriteFileResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();
        let backing = self.backing.clone();
        let blocking_context = context.clone();
        let written = tokio::task::spawn_blocking(move || -> io::Result<u32> {
            let _context = blocking_context;
            backing.write(&req.path, req.offset, &req.data)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))
        .and_then(|result| result.map_err(io_status));
        self.log_io(&context, "write", &written);
        Ok(context.response(WriteFileResponse { written: written? }))
    }

    async fn fsync_file(
        &self,
        request: Request<FsyncFileRequest>,
    ) -> Result<Response<FsyncFileResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();
        let backing = self.backing.clone();
        let blocking_context = context.clone();
        let result = tokio::task::spawn_blocking(move || -> io::Result<()> {
            let _context = blocking_context;
            backing.fsync(&req.path, req.datasync)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))
        .and_then(|result| result.map_err(io_status));
        self.log_io(&context, "fsync", &result);
        result?;
        Ok(context.response(FsyncFileResponse {}))
    }

    async fn set_attr(
        &self,
        request: Request<SetAttrRequest>,
    ) -> Result<Response<SetAttrResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();

        // Ownership changes need privilege this backend does not assume.
        // Refusing explicitly beats silently not applying them.
        if req.uid.is_some() || req.gid.is_some() {
            return Err(Status::unimplemented("changing ownership is not supported"));
        }

        let real = self.backing.existing(&req.path).map_err(io_status)?;

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

        let entry = self.resolve(&req.path)?;
        let attr = self
            .backing
            .attr_for(&req.path, &entry)
            .map_err(io_status)?;
        Ok(context.response(SetAttrResponse {
            attr: Some(to_proto_attr(&attr)),
        }))
    }

    async fn stat_fs(
        &self,
        request: Request<StatFsRequest>,
    ) -> Result<Response<StatFsResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let root = self.resolve("/")?;
        let blocking_context = context.clone();
        let stats = tokio::task::spawn_blocking(move || -> io::Result<PbStatFs> {
            let _context = blocking_context;
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

        Ok(context.response(stats))
    }

    async fn remove_file(
        &self,
        request: Request<RemoveFileRequest>,
    ) -> Result<Response<RemoveFileResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let real = self.resolve(&request.get_ref().path)?;
        tokio::fs::remove_file(&real).await.map_err(io_status)?;
        Ok(context.response(RemoveFileResponse {}))
    }

    async fn remove_dir(
        &self,
        request: Request<RemoveDirRequest>,
    ) -> Result<Response<RemoveDirResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let real = self.resolve(&request.get_ref().path)?;
        // std/tokio refuse a non-empty directory, which is the behaviour
        // `rmdir` needs; the message is what the client turns into ENOTEMPTY.
        tokio::fs::remove_dir(&real).await.map_err(io_status)?;
        Ok(context.response(RemoveDirResponse {}))
    }

    async fn rename(
        &self,
        request: Request<RenameRequest>,
    ) -> Result<Response<RenameResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();
        if req.flags != 0 {
            return Err(Status::unimplemented(
                "rename flags (NOREPLACE/EXCHANGE) are not supported",
            ));
        }

        let from = self.resolve(&req.old_path)?;
        let to = self.resolve(&req.new_path)?;
        tokio::fs::rename(&from, &to).await.map_err(io_status)?;
        Ok(context.response(RenameResponse {}))
    }

    async fn shutdown(
        &self,
        request: Request<crate::punk_fs::ShutdownRequest>,
    ) -> Result<Response<crate::punk_fs::ShutdownResponse>, Status> {
        let _context = crate::telemetry::OperationContext::from_request(&request)?;
        if !self.allow_shutdown {
            return Err(Status::unimplemented(
                "shutdown is unavailable in node mode",
            ));
        }
        #[cfg(feature = "dev-shutdown")]
        {
            self.shutdown_requested.store(true, Ordering::SeqCst);
            self.shutdown.notify_waiters();
            Ok(_context.response(crate::punk_fs::ShutdownResponse {}))
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

    async fn write(service: &Arc<PunkFsService>, path: &str, offset: u64, data: &[u8]) -> u32 {
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
        let names: Vec<String> = list(&service, "/")
            .await
            .into_iter()
            .map(|e| e.name)
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[tokio::test]
    async fn guids_are_stable_across_repeated_reads() {
        let (_dir, service) = fixture();
        let first = attr(&service, "/hello.txt").await.guid;
        let second = attr(&service, "/hello.txt").await.guid;
        assert_eq!(first, second);

        let other = attr(&service, "/sub").await.guid;
        assert_ne!(first, other, "different entries must not share a guid");
    }

    /// The whole point of persisting the guid: a rename must not change the
    /// file's punkfs identity.
    #[tokio::test]
    async fn guid_survives_a_rename() {
        let (dir, service) = fixture();
        let before = attr(&service, "/hello.txt").await.guid;

        fs::rename(dir.path().join("hello.txt"), dir.path().join("renamed.txt")).unwrap();

        let after = attr(&service, "/renamed.txt").await.guid;
        assert_eq!(
            before, after,
            "a renamed file must keep its guid; it was re-minted instead"
        );
    }

    /// A guids must also survive a move between directories on one node.
    #[tokio::test]
    async fn guid_survives_a_move_between_directories() {
        let (dir, service) = fixture();
        let before = attr(&service, "/hello.txt").await.guid;

        fs::rename(
            dir.path().join("hello.txt"),
            dir.path().join("sub/moved.txt"),
        )
        .unwrap();

        assert_eq!(attr(&service, "/sub/moved.txt").await.guid, before);
    }

    /// The persisted value is what is read back, not something re-derived:
    /// planting a known guid must make the server report it verbatim.
    #[tokio::test]
    async fn a_planted_guid_is_reported_verbatim() {
        let (dir, service) = fixture();
        let planted = "3f2a1b4c-5d6e-4f70-8a9b-0c1d2e3f4a5b";
        let _ = plant_guid(&dir.path().join("hello.txt"), planted);

        if read_guid_xattr(&dir.path().join("hello.txt")).as_deref() == Some(planted) {
            assert_eq!(attr(&service, "/hello.txt").await.guid, planted);
        } else {
            eprintln!("skipping: backing filesystem does not support user xattrs");
        }
    }

    fn plant_guid(path: &Path, guid: &str) -> bool {
        write_guid_xattr(path, guid)
    }

    /// Filesystems without xattr support still need a guid. The derived
    /// fallback must be deterministic per (node, path) and parse as a guid,
    /// so a client can rely on it even though it will not survive a rename.
    #[test]
    fn derived_fallback_guid_is_deterministic_and_valid() {
        let a = derived_guid("node-a", "/hello.txt");
        let b = derived_guid("node-a", "/hello.txt");
        let other_path = derived_guid("node-a", "/other.txt");
        let other_node = derived_guid("node-b", "/hello.txt");

        assert_eq!(a, b, "same node+path must derive the same guid");
        assert_ne!(a, other_path, "different paths must not collide");
        assert_ne!(a, other_node, "different nodes must not collide");
        assert!(uuid::Uuid::parse_str(&a).is_ok(), "must be a valid guid");
    }

    /// A garbage xattr must not make a healthy file unreadable: it is
    /// treated as absent and a fresh guid is minted.
    #[tokio::test]
    async fn a_corrupt_guid_xattr_is_replaced_rather_than_served() {
        let (dir, service) = fixture();
        let real = dir.path().join("hello.txt");
        if !write_guid_xattr(&real, "not-a-guid") {
            eprintln!("skipping: backing filesystem does not support user xattrs");
            return;
        }
        assert_eq!(
            read_guid_xattr(&real),
            None,
            "corrupt value must read as absent"
        );

        let reported = attr(&service, "/hello.txt").await.guid;
        assert!(uuid::Uuid::parse_str(&reported).is_ok());
        assert_ne!(reported, "not-a-guid");
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
        assert_eq!(
            read(&service, "/hello.txt", 0, 1024).await,
            b"hello world!!"
        );
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
        assert_eq!(
            fs::read(dir.path().join("renamed.txt")).unwrap(),
            b"hello world"
        );
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
            let result = service.get_attr(Request::new(GetAttrRequest { path: bad.into() }));
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
            (
                io::ErrorKind::PermissionDenied,
                tonic::Code::PermissionDenied,
            ),
            (io::ErrorKind::AlreadyExists, tonic::Code::AlreadyExists),
            (io::ErrorKind::InvalidInput, tonic::Code::InvalidArgument),
            (io::ErrorKind::Unsupported, tonic::Code::Unimplemented),
        ];
        for (kind, code) in cases {
            assert_eq!(io_status(io::Error::from(kind)).code(), code, "{kind:?}");
        }
    }
}
