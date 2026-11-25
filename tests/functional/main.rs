//! Functional tests: exercise a whole operation end to end and assert the
//! *desired* outcome, driving the real client `Store`/`MetadataCache` against
//! a live `punkfs-server` process (the out-of-process "mock system"). No FUSE
//! mount here — the server process stands in for the network/system boundary
//! the integration suite later mounts.
//!
//! The endgame behaviour under test: one folder is assembled from several
//! remote backing folders, and a change on one backing folder is present and
//! viewable in the merged view.

#[path = "../common/mod.rs"]
mod common;

#[path = "../posix_contract/mod.rs"]
mod shared_posix_contract;

mod correlation;
mod merge;
mod metadata_cache;
mod multinode;
mod peers;
mod posix_contract;
mod store;
mod store_test_api;
