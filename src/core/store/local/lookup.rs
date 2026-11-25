//! Obtain local metadata with the same GUID rules as the backing server.
use super::Local;
use crate::core::metadata::{Availability, FsEntry};
use crate::store::StoreError;

pub(super) fn entry(local: &Local, path: &str) -> Result<FsEntry, StoreError> {
    let attr = local
        .backing
        .attr_for(path, &local.backing.entry_path(path).map_err(super::error)?)
        .map_err(super::error)?;
    Ok(FsEntry {
        inode: 0,
        path: path.into(),
        guid: attr.guid.clone(),
        kind: attr.kind,
        node_id: local.backing.node_id().to_owned(),
        availability: Availability::Available,
        attr,
    })
}
