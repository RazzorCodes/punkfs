//! Op-level errors: what a FUSE method can tell the kernel.
//!
//! The op modules do their work against plain values (see the `*_decision`
//! functions in each module) and report problems as [`OpError`]; the thin
//! FUSE adapters translate that into a `fuser::Errno`. Keeping this out of
//! the store means `StoreError` (catalog/transport) and `OpError` (syscall
//! semantics) can evolve separately.
use fuser::Errno;

use crate::store::StoreError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpError {
    /// No such file, directory, or inode (ENOENT).
    NotFound,
    /// Directory still has children (ENOTEMPTY).
    NotEmpty,
    /// Operation refused by policy (EPERM).
    NotPermitted,
    /// Bad argument, e.g. a non-UTF8 name (EINVAL).
    InvalidInput,
    /// The op exists but its backing rpc does not yet (ENOSYS).
    Unimplemented,
    /// Anything else from the store/transport (EIO).
    Io(String),
}

impl OpError {
    pub(crate) fn errno(&self) -> Errno {
        match self {
            OpError::NotFound => Errno::ENOENT,
            OpError::NotEmpty => Errno::ENOTEMPTY,
            OpError::NotPermitted => Errno::EPERM,
            OpError::InvalidInput => Errno::EINVAL,
            OpError::Unimplemented => Errno::ENOSYS,
            OpError::Io(_) => Errno::EIO,
        }
    }
}

impl From<StoreError> for OpError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound => OpError::NotFound,
            StoreError::NotPermitted => OpError::NotPermitted,
            StoreError::Unimplemented => OpError::Unimplemented,
            StoreError::Io(msg) => OpError::Io(msg),
        }
    }
}

pub(crate) type OpResult<T> = Result<T, OpError>;
