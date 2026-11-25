//! Routing and metadata components shared by the filesystem and peer service.
//!
//! `store` routes operations, `cache` keeps hot catalog data in memory, and
//! [`crate::db`] persists the catalog. `metadata` and `types` hold shared types
//! rather than making the durable database depend on the cache.
pub(crate) mod backing;
pub mod cache;
pub mod config;
pub mod metadata;
pub mod node;
pub mod punkfs;
pub mod store;
pub mod types;
