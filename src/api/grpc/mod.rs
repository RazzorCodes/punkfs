//! grpc side of punkfs: the service implementing the generated protocol.
//!
//! The generated message/service bindings themselves are re-exported at the
//! crate root as [`crate::punk_fs`].
pub(crate) mod peer;
pub mod service;
