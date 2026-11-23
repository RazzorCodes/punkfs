//! Resolve a logical path relative to the pre-mount backing descriptor.
use super::Local;
use crate::core::backing::SafePath;
use crate::store::StoreError;

pub(super) fn real(local: &Local, path: &str) -> Result<SafePath, StoreError> {
    local.backing.path(path).map_err(super::error)
}
