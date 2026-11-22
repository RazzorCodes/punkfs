//! Read local directory entries before reconciling the merged catalog.
use super::Local;
use crate::store::StoreError;
use crate::store::db::metadata_cache::FsEntry;
use std::fs;

pub(super) fn list(local: &Local, dir: &str) -> Result<Vec<FsEntry>, StoreError> {
    let mut entries = Vec::new();
    for child in fs::read_dir(local.real(dir)?).map_err(super::error)? {
        let child = child.map_err(super::error)?;
        let name = child
            .file_name()
            .into_string()
            .map_err(|_| StoreError::Local(libc::EINVAL))?;
        let path = if dir == "/" {
            format!("/{name}")
        } else {
            format!("{}/{name}", dir.trim_end_matches('/'))
        };
        entries.push(local.entry(&path)?);
    }
    Ok(entries)
}
