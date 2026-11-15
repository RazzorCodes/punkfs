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
    /// Operation refused (EPERM).
    NotPermitted,
    /// Bad argument, e.g. a non-UTF8 name (EINVAL).
    InvalidInput,
    /// The operation is not supported by this build/backend (ENOSYS).
    Unsupported,
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
            OpError::Unsupported => Errno::ENOSYS,
            OpError::Io(_) => Errno::EIO,
        }
    }
}

impl From<StoreError> for OpError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound => OpError::NotFound,
            StoreError::NotPermitted => OpError::NotPermitted,
            StoreError::Unsupported(_) => OpError::Unsupported,
            StoreError::Io(msg) => OpError::Io(msg),
        }
    }
}

pub(crate) type OpResult<T> = Result<T, OpError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_errors_map_to_syscall_errors() {
        assert_eq!(OpError::from(StoreError::NotFound), OpError::NotFound);
        assert_eq!(
            OpError::from(StoreError::NotPermitted),
            OpError::NotPermitted
        );
        assert_eq!(
            OpError::from(StoreError::Unsupported("nope".into())),
            OpError::Unsupported
        );
        assert_eq!(
            OpError::from(StoreError::Io("down".into())),
            OpError::Io("down".into())
        );
    }

    #[test]
    fn errnos_cover_every_variant() {
        assert_eq!(OpError::NotFound.errno(), Errno::ENOENT);
        assert_eq!(OpError::NotEmpty.errno(), Errno::ENOTEMPTY);
        assert_eq!(OpError::NotPermitted.errno(), Errno::EPERM);
        assert_eq!(OpError::InvalidInput.errno(), Errno::EINVAL);
        assert_eq!(OpError::Unsupported.errno(), Errno::ENOSYS);
        assert_eq!(OpError::Io("x".into()).errno(), Errno::EIO);
    }
}
