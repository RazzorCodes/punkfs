//! punkfs: the amalgamation — fs (fuse), grpc, db, cache.
mod cache;
mod db;
mod filesystem;

pub use filesystem::Fs as PunkFs;

pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(0);

#[path = "client/grpc/gen/punk_fs.rs"]
pub mod punk_fs;
