//! The grpc service: one backing folder served over grpc.
//!
//! This lives in the library (not the binary) so integration tests can
//! construct it over a `TempDir` and call handlers directly, or serve it on
//! an ephemeral port.
//!
//! Scope: this iteration serves content read/write, attributes, and removal
//! for a *single* backing folder on the local machine. Remote creation is
//! absent; combined nodes create entries on their local backing through FUSE.
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::UNIX_EPOCH;

use tokio::sync::Notify;
use tonic::{Request, Response, Status};

use crate::core::backing::{Backing, SafePath};
#[cfg(test)]
use crate::core::backing::{derived_guid, read_guid_xattr, write_guid_xattr};
use crate::core::types::misc::xattr;
use crate::core::types::{FileAttrData, FsEntryKind};
use crate::punk_fs::punk_fs_server::PunkFs;
use crate::punk_fs::{
    Access as PbAccess, DirEntry, FileAttr, FileKind, FileTimes as PbFileTimes, FsyncFileRequest,
    FsyncFileResponse, GetAttrRequest, GetAttrResponse, ListDirectoryRequest,
    ListDirectoryResponse, ReadFileRequest, ReadFileResponse, ReadLinkRequest, ReadLinkResponse,
    RemoveDirRequest, RemoveDirResponse, RemoveFileRequest, RemoveFileResponse, RenameRequest,
    RenameResponse, SetAttrRequest, SetAttrResponse, Stat as PbStat, StatFsRequest, StatFsResponse,
    StatFsResponse as PbStatFs, WriteFileRequest, WriteFileResponse,
};
use crate::store::Store;

/// Upper bound on a single `ReadFile` answer. The FUSE layer asks for
/// whatever the kernel wants (commonly 128 KiB); this keeps one malicious
/// request from asking for a gigabyte.
pub const MAX_READ: u32 = 1024 * 1024;

/// A served folder, plus the hooks `main` needs to run and stop it.
pub struct PunkFsService {
    backing: Arc<Backing>,
    allow_shutdown: bool,
    catalog: Option<Arc<Store>>,
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
        Self::from_backing_with_catalog(backing, allow_shutdown, None)
    }

    pub(crate) fn from_backing_with_catalog(
        backing: Arc<Backing>,
        allow_shutdown: bool,
        catalog: Option<Arc<Store>>,
    ) -> Arc<Self> {
        Arc::new(PunkFsService {
            backing,
            allow_shutdown,
            catalog,
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
        FsEntryKind::Fifo => FileKind::Fifo,
        FsEntryKind::Socket => FileKind::Socket,
        FsEntryKind::BlockDevice => FileKind::BlockDevice,
        FsEntryKind::CharDevice => FileKind::CharDevice,
        FsEntryKind::Unknown | FsEntryKind::File => FileKind::File,
    }
}

pub(crate) fn to_proto_attr(data: &FileAttrData) -> FileAttr {
    let parts = |t: std::time::SystemTime| match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            if d.subsec_nanos() == 0 {
                (-(d.as_secs() as i64), 0)
            } else {
                (-(d.as_secs() as i64) - 1, 1_000_000_000 - d.subsec_nanos())
            }
        }
    };
    let (accessed_unix, accessed_nanos) = parts(data.times.accessed);
    let (modified_unix, modified_nanos) = parts(data.times.modified);
    let (changed_unix, changed_nanos) = parts(data.times.changed);
    let (created_unix, created_nanos) = parts(data.times.created);

    FileAttr {
        guid: data.guid.clone(),
        kind: file_kind(data.kind) as i32,
        times: Some(PbFileTimes {
            accessed_unix,
            modified_unix,
            changed_unix,
            created_unix,
            accessed_nanos,
            modified_nanos,
            changed_nanos,
            created_nanos,
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
            rdev: data.stat.rdev,
            nlink: data.stat.nlink,
            flags: data.stat.flags,
        }),
    }
}

/// Map an `io::Error` onto the grpc code the client will translate back
/// into an errno. The mapping is deliberately narrow: anything the client
/// cannot act on becomes `internal`.
pub fn io_status(err: io::Error) -> Status {
    if matches!(
        err.raw_os_error(),
        Some(libc::ENOTEMPTY) | Some(libc::EEXIST) | Some(libc::EINVAL) | Some(libc::EXDEV)
    ) {
        let errno = err.raw_os_error().expect("checked above");
        let mut status = if errno == libc::EINVAL {
            Status::invalid_argument(err.to_string())
        } else {
            Status::failed_precondition(err.to_string())
        };
        status.metadata_mut().insert(
            "x-punkfs-errno",
            errno.to_string().parse().expect("errno metadata"),
        );
        return status;
    }
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

fn xattr_status(err: io::Error) -> Status {
    if let Some(errno) = err.raw_os_error() {
        let mut status = io_status(err);
        if let Ok(value) = errno.to_string().parse() {
            status.metadata_mut().insert("x-punkfs-errno", value);
        }
        status
    } else {
        io_status(err)
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

        while let Some(entry) = rd.next_entry().await.map_err(io_status)? {
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
        let real = self.backing.entry_path(&logical).map_err(io_status)?;
        let attr = self.backing.attr_for(&logical, &real).map_err(io_status)?;
        Ok(context.response(GetAttrResponse {
            attr: Some(to_proto_attr(&attr)),
        }))
    }

    async fn read_link(
        &self,
        request: Request<ReadLinkRequest>,
    ) -> Result<Response<ReadLinkResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let logical = request.into_inner().path;
        let backing = self.backing.clone();
        let result = tokio::task::spawn_blocking(move || backing.readlink(&logical))
            .await
            .map_err(|_| Status::internal("readlink worker failed"))
            .and_then(|result| {
                result.map_err(|err| {
                    if err.raw_os_error() == Some(libc::EINVAL) {
                        Status::invalid_argument("not a symlink")
                    } else {
                        io_status(err)
                    }
                })
            });
        self.log_io(&context, "readlink", &result);
        Ok(context.response(ReadLinkResponse { target: result? }))
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
        let result = (|| {
            if req.accessed_nanos.is_some_and(|n| n >= 1_000_000_000)
                || req.modified_nanos.is_some_and(|n| n >= 1_000_000_000)
            {
                return Err(Status::invalid_argument("invalid nanoseconds"));
            }
            let real = self.backing.existing(&req.path).map_err(io_status)?;
            if req.uid.is_some() || req.gid.is_some() {
                let path = std::ffi::CString::new(real.as_os_str().as_encoded_bytes())
                    .map_err(|_| Status::invalid_argument("path contains NUL"))?;
                if unsafe {
                    libc::chown(
                        path.as_ptr(),
                        req.uid.unwrap_or(u32::MAX),
                        req.gid.unwrap_or(u32::MAX),
                    )
                } != 0
                {
                    return Err(io_status(io::Error::last_os_error()));
                }
            }

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
                apply_times(
                    &real,
                    req.accessed_unix,
                    req.accessed_nanos,
                    req.modified_unix,
                    req.modified_nanos,
                )?;
            }

            let entry = self.resolve(&req.path)?;
            let attr = self
                .backing
                .attr_for(&req.path, &entry)
                .map_err(io_status)?;
            Ok::<_, Status>(SetAttrResponse {
                attr: Some(to_proto_attr(&attr)),
            })
        })();
        self.log_io(&context, "setattr", &result);
        Ok(context.response(result?))
    }

    async fn get_xattr(
        &self,
        request: Request<crate::punk_fs::XattrRequest>,
    ) -> Result<Response<crate::punk_fs::XattrResponse>, Status> {
        let ctx = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();
        let result = self
            .backing
            .entry_path(&req.path)
            .map_err(io_status)
            .and_then(|path| xattr::get(&path, &req.name).map_err(xattr_status));
        self.log_io(&ctx, "getxattr", &result);
        Ok(ctx.response(crate::punk_fs::XattrResponse { value: result? }))
    }
    async fn list_xattr(
        &self,
        request: Request<crate::punk_fs::XattrRequest>,
    ) -> Result<Response<crate::punk_fs::XattrResponse>, Status> {
        let ctx = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();
        let result = self
            .backing
            .entry_path(&req.path)
            .map_err(io_status)
            .and_then(|path| xattr::list(&path).map_err(xattr_status));
        self.log_io(&ctx, "listxattr", &result);
        Ok(ctx.response(crate::punk_fs::XattrResponse { value: result? }))
    }
    async fn set_xattr(
        &self,
        request: Request<crate::punk_fs::XattrRequest>,
    ) -> Result<Response<crate::punk_fs::XattrResponse>, Status> {
        let ctx = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();
        let result = self
            .backing
            .entry_path(&req.path)
            .map_err(io_status)
            .and_then(|path| {
                xattr::set(&path, &req.name, &req.value, req.flags).map_err(xattr_status)
            });
        self.log_io(&ctx, "setxattr", &result);
        result?;
        Ok(ctx.response(crate::punk_fs::XattrResponse { value: vec![] }))
    }
    async fn remove_xattr(
        &self,
        request: Request<crate::punk_fs::XattrRequest>,
    ) -> Result<Response<crate::punk_fs::XattrResponse>, Status> {
        let ctx = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();
        let result = self
            .backing
            .entry_path(&req.path)
            .map_err(io_status)
            .and_then(|path| xattr::remove(&path, &req.name).map_err(xattr_status));
        self.log_io(&ctx, "removexattr", &result);
        result?;
        Ok(ctx.response(crate::punk_fs::XattrResponse { value: vec![] }))
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
        let real = self
            .backing
            .entry_path(&request.get_ref().path)
            .map_err(io_status)?;
        tokio::fs::remove_file(&real).await.map_err(io_status)?;
        if let Some(store) = &self.catalog {
            store
                .remove_local_in(&context, &request.get_ref().path)
                .map_err(|e| {
                    Status::internal(format!("catalog update after remove failed: {e:?}"))
                })?;
        }
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
        if let Some(store) = &self.catalog {
            store
                .remove_local_in(&context, &request.get_ref().path)
                .map_err(|e| {
                    Status::internal(format!("catalog update after rmdir failed: {e:?}"))
                })?;
        }
        Ok(context.response(RemoveDirResponse {}))
    }

    async fn rename(
        &self,
        request: Request<RenameRequest>,
    ) -> Result<Response<RenameResponse>, Status> {
        let context = crate::telemetry::OperationContext::from_request(&request)?;
        let req = request.into_inner();
        let result = (|| {
            if let Some(store) = &self.catalog {
                store
                    .validate_owner_rename_in(
                        &context,
                        &req.old_path,
                        &req.new_path,
                        self.backing.node_id(),
                    )
                    .map_err(|error| match error {
                        crate::StoreError::Local(errno) => {
                            io_status(io::Error::from_raw_os_error(errno))
                        }
                        other => {
                            Status::internal(format!("catalog rename validation failed: {other:?}"))
                        }
                    })?;
            }
            let from = self.backing.entry_path(&req.old_path).map_err(io_status)?;
            let to = self.backing.entry_path(&req.new_path).map_err(io_status)?;
            let from = std::ffi::CString::new(from.as_os_str().as_encoded_bytes())
                .map_err(|_| Status::invalid_argument("path contains NUL"))?;
            let to = std::ffi::CString::new(to.as_os_str().as_encoded_bytes())
                .map_err(|_| Status::invalid_argument("path contains NUL"))?;
            if unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    from.as_ptr(),
                    libc::AT_FDCWD,
                    to.as_ptr(),
                    req.flags,
                )
            } != 0
            {
                return Err(io_status(io::Error::last_os_error()));
            }
            if let Some(store) = &self.catalog {
                store
                    .update_local_rename_in(
                        &context,
                        &req.old_path,
                        &req.new_path,
                        fuser::RenameFlags::from_bits_retain(req.flags),
                    )
                    .map_err(|e| {
                        Status::internal(format!("catalog update after rename failed: {e:?}"))
                    })?;
            }
            Ok::<_, Status>(RenameResponse {})
        })();
        self.log_io(&context, "rename", &result);
        Ok(context.response(result?))
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
    accessed_nanos: Option<u32>,
    modified_unix: Option<i64>,
    modified_nanos: Option<u32>,
) -> Result<(), Status> {
    if accessed_nanos.is_some_and(|n| n >= 1_000_000_000)
        || modified_nanos.is_some_and(|n| n >= 1_000_000_000)
    {
        return Err(Status::invalid_argument("invalid nanoseconds"));
    }
    let to_timespec = |secs: Option<i64>, nanos: Option<u32>| libc::timespec {
        tv_sec: secs.unwrap_or(0) as libc::time_t,
        tv_nsec: if secs.is_some() {
            nanos.unwrap_or(0) as libc::c_long
        } else {
            libc::UTIME_OMIT
        },
    };
    let times = [
        to_timespec(accessed_unix, accessed_nanos),
        to_timespec(modified_unix, modified_nanos),
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

    fn plant_guid(path: &Path, guid: &str) -> bool {
        write_guid_xattr(path, guid)
    }

    /// The punkfs identity is persisted on the entry so it survives rename,
    /// and is reported verbatim rather than re-derived on every read.
    #[tokio::test]
    async fn guids_are_stable_and_persist_across_renames_and_moves() {
        let (dir, service) = fixture();
        let first = attr(&service, "/hello.txt").await.guid;
        assert_eq!(attr(&service, "/hello.txt").await.guid, first);
        assert_ne!(attr(&service, "/sub").await.guid, first);

        fs::rename(dir.path().join("hello.txt"), dir.path().join("renamed.txt")).unwrap();
        assert_eq!(attr(&service, "/renamed.txt").await.guid, first);

        fs::rename(
            dir.path().join("renamed.txt"),
            dir.path().join("sub/moved.txt"),
        )
        .unwrap();
        assert_eq!(attr(&service, "/sub/moved.txt").await.guid, first);
    }

    /// The persisted xattr is what is served (not a re-derivation), and a
    /// corrupt value is treated as absent rather than making the file
    /// unreadable.
    #[tokio::test]
    async fn planted_and_corrupt_guid_xattrs() {
        let (dir, service) = fixture();
        let planted = "3f2a1b4c-5d6e-4f70-8a9b-0c1d2e3f4a5b";
        if plant_guid(&dir.path().join("hello.txt"), planted)
            && read_guid_xattr(&dir.path().join("hello.txt")).as_deref() == Some(planted)
        {
            assert_eq!(attr(&service, "/hello.txt").await.guid, planted);
        }

        let corrupt = dir.path().join("hello.txt");
        if write_guid_xattr(&corrupt, "not-a-guid") {
            assert_eq!(read_guid_xattr(&corrupt), None);
            let reported = attr(&service, "/hello.txt").await.guid;
            assert!(uuid::Uuid::parse_str(&reported).is_ok());
            assert_ne!(reported, "not-a-guid");
        }
    }

    /// Filesystems without xattr support get a deterministic derived guid
    /// that still parses as one.
    #[test]
    fn derived_fallback_guid_is_deterministic_and_valid() {
        let a = derived_guid("node-a", "/hello.txt");
        let b = derived_guid("node-a", "/hello.txt");
        let other_path = derived_guid("node-a", "/other.txt");
        let other_node = derived_guid("node-b", "/hello.txt");

        assert_eq!(a, b);
        assert_ne!(a, other_path);
        assert_ne!(a, other_node);
        assert!(uuid::Uuid::parse_str(&a).is_ok());
    }

    #[tokio::test]
    async fn resolve_rejects_traversal_and_absolute_paths() {
        let (_dir, service) = fixture();
        for bad in ["../etc/passwd", "/..", "a/../../b"] {
            let err = service
                .get_attr(Request::new(GetAttrRequest { path: bad.into() }))
                .await
                .unwrap_err();
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
