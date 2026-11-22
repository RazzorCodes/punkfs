//! Resolve a logical path relative to the pre-mount directory descriptor.
use super::Local;
use crate::store::StoreError;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};

pub(super) fn real(local: &Local, path: &str) -> Result<PathBuf, StoreError> {
    let path = Path::new(path.trim_start_matches('/'));
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(StoreError::Local(libc::EINVAL));
    }
    Ok(PathBuf::from(format!("/proc/self/fd/{}", local.root.as_raw_fd())).join(path))
}
