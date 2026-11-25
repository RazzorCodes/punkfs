//! Routing and metadata components shared by the filesystem and peer service.
//!
//! `store` routes operations, `cache` keeps hot catalog data in memory, and
//! [`crate::db`] persists the catalog, and `types` holds shared types
//! without making the durable database depend on the cache.
pub(crate) mod backing;
pub mod cache;
pub mod store;
pub mod types;
