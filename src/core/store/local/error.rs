//! Preserve the errno returned by a local filesystem operation.
use crate::store::StoreError;

pub(super) fn from_io(error: std::io::Error) -> StoreError {
    StoreError::Local(error.raw_os_error().unwrap_or(libc::EIO))
}
