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
    /// The node owning the entry did not answer (EHOSTDOWN).
    ///
    /// Distinct from `Io`/`EIO` on purpose: "the machine holding these
    /// bytes is unreachable" is a transient, actionable condition, and a
    /// caller must be able to tell it apart from a disk-level failure.
    Inaccessible,
    /// A move would cross backing nodes (EXDEV).
    ///
    /// The same errno a local filesystem reports for a rename across
    /// devices, because that is exactly what this is.
    CrossDevice,
    /// Anything else from the store/transport (EIO).
    Io(String),
    Local(i32),
}

impl OpError {
    pub(crate) fn errno(&self) -> Errno {
        match self {
            OpError::NotFound => Errno::ENOENT,
            OpError::NotEmpty => Errno::ENOTEMPTY,
            OpError::NotPermitted => Errno::EPERM,
            OpError::InvalidInput => Errno::EINVAL,
            OpError::Unsupported => Errno::ENOSYS,
            OpError::Inaccessible => Errno::EHOSTDOWN,
            OpError::CrossDevice => Errno::EXDEV,
            OpError::Io(_) => Errno::EIO,
            OpError::Local(errno) => Errno::from_i32(*errno),
        }
    }
}

impl From<StoreError> for OpError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound => OpError::NotFound,
            StoreError::NotPermitted => OpError::NotPermitted,
            StoreError::Unsupported(_) => OpError::Unsupported,
            StoreError::Inaccessible(_) => OpError::Inaccessible,
            StoreError::Io(msg) => OpError::Io(msg),
            StoreError::Local(errno) => OpError::Local(errno),
        }
    }
}

pub(crate) type OpResult<T> = Result<T, OpError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Store-level failures map to the syscall errno the kernel expects,
    /// and an unreachable node is a distinct, actionable EHOSTDOWN rather
    /// than a generic EIO.
    #[test]
    fn store_errors_map_to_distinct_syscall_errnos() {
        let cases = [
            (OpError::from(StoreError::NotFound), Errno::ENOENT),
            (OpError::from(StoreError::NotPermitted), Errno::EPERM),
            (
                OpError::from(StoreError::Unsupported("n".into())),
                Errno::ENOSYS,
            ),
            (
                OpError::from(StoreError::Inaccessible("node-a".into())),
                Errno::EHOSTDOWN,
            ),
            (OpError::from(StoreError::Io("down".into())), Errno::EIO),
            (OpError::NotEmpty, Errno::ENOTEMPTY),
            (OpError::InvalidInput, Errno::EINVAL),
            (OpError::CrossDevice, Errno::EXDEV),
        ];
        for (op, errno) in cases {
            assert_eq!(op.errno(), errno);
        }
    }
}
