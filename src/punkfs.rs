//! punkfs: the amalgamation — fs (fuse), grpc, db, store.
pub mod db;
mod filesystem;
mod store;

pub use db::fstree::FsNode;
pub use filesystem::Fs as PunkFs;

pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(0);

#[path = "api/grpc/gen/punk_fs.rs"]
pub mod punk_fs;
