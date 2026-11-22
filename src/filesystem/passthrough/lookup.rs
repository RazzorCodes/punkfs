//! Obtain local metadata with the same GUID rules as the backing server.
use super::Local;
use crate::store::StoreError;
use crate::store::db::metadata_cache::{Availability, FsEntry};

pub(super) fn entry(local: &Local, path: &str) -> Result<FsEntry, StoreError> {
    let attr = local
        .attrs
        .attr_for(path, &local.real(path)?)
        .map_err(super::error)?;
    Ok(FsEntry {
        inode: 0,
        path: path.into(),
        guid: attr.guid.clone(),
        kind: attr.kind,
        node_id: local.node.clone(),
        availability: Availability::Available,
        attr,
    })
}
