//! punkfs: the amalgamation — fs (fuse), grpc, store.
pub mod core;
pub mod db;
mod filesystem;
pub use core::store;
#[path = "api/grpc/mod.rs"]
pub mod grpc;
pub mod telemetry;

pub use core::cache::metadata::FsNode;
pub use core::punkfs::Fs as PunkFs;
pub use store::{SetattrChanges, StatFs, Store, StoreApi, StoreChild, StoreError};

pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(0);

#[path = "api/grpc/gen/punk_fs.rs"]
pub mod punk_fs;
