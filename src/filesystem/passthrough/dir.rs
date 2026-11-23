//! Read local directory entries before reconciling the merged catalog.
use super::Local;
use crate::store::StoreError;
use crate::store::db::metadata_cache::FsEntry;
use std::fs;

pub(super) fn list(
    local: &Local,
    ctx: &crate::telemetry::OperationContext,
    dir: &str,
) -> Result<Vec<FsEntry>, StoreError> {
    let mut entries = Vec::new();
    let mut skipped = 0;
    let directory = local.backing.existing(dir).map_err(super::error)?;
    for child in fs::read_dir(&directory).map_err(super::error)? {
        let child = child.map_err(super::error)?;
        if child.file_type().map_err(super::error)?.is_symlink() {
            skipped += 1;
            continue;
        }
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
    local.backing.log_skipped_symlinks(ctx, skipped);
    Ok(entries)
}
