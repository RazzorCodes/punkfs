//! Local symlink operations.
use crate::store::StoreError;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(super) fn readlink(path: &Path) -> Result<Vec<u8>, StoreError> {
    std::fs::read_link(path)
        .map(|target| target.as_os_str().as_bytes().to_vec())
        .map_err(super::error)
}
