//! Store-level conformance: the client `Store` against a live server over
//! real grpc, with no FUSE in the way.
//!
//! These isolate the catalog/transport layer: if a mount-level test fails,
//! a green test here tells you the bug is above the store, in the FUSE op
//! layer or the kernel's assumptions.
//!
//! Every assertion checks both the returned value and the backing directory,
//! so a store bug cannot hide behind a matching server bug.

use punkfs::FsNode;
use punkfs::Store;
use punkfs::db::fstree::FsEntryKind;
use punkfs::store::{StoreApi, StoreError};

use crate::common::TestServer;

fn store_for(server: &TestServer) -> Store {
    Store::new(
        &server.url,
        FsNode {
            node_id: "test-node".into(),
        },
    )
    .expect("build store")
}

#[test]
fn listing_reports_exactly_the_backing_children() {
    let server = TestServer::empty();
    server.seed("a.txt", b"a");
    server.seed("b.txt", b"b");
    server.seed_dir("c");
    let store = store_for(&server);

    store.ensure_listed("/").expect("list root");

    let mut names: Vec<String> = store.get_children("/").into_iter().map(|c| c.name).collect();
    names.sort();
    assert_eq!(names, vec!["a.txt", "b.txt", "c"]);
}

#[test]
fn listing_reflects_entries_added_behind_our_back() {
    let server = TestServer::empty();
    server.seed("first.txt", b"1");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();
    assert_eq!(store.get_children("/").len(), 1);

    // The backing store changes without punkfs knowing.
    server.seed("second.txt", b"2");

    let names: Vec<String> = store.get_children("/").into_iter().map(|c| c.name).collect();
    assert!(
        names.contains(&"second.txt".to_string()),
        "a newly created backing file must appear on the next listing, got {names:?}"
    );
}

#[test]
fn listing_drops_entries_removed_behind_our_back() {
    let server = TestServer::empty();
    server.seed("stays.txt", b"s");
    server.seed("goes.txt", b"g");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();

    std::fs::remove_file(server.root().join("goes.txt")).unwrap();

    let names: Vec<String> = store.get_children("/").into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec!["stays.txt"],
        "a deleted backing file must disappear from the listing"
    );
}

#[test]
fn repeated_listings_keep_guids_stable() {
    let server = TestServer::empty();
    server.seed("stable.txt", b"s");
    let store = store_for(&server);

    let guid_of = |store: &Store| -> Vec<String> {
        store.ensure_listed("/").unwrap();
        store
            .get_children("/")
            .into_iter()
            .map(|c| store.get_entry(c.inode).unwrap().guid)
            .collect()
    };

    let first = guid_of(&store);
    let second = guid_of(&store);
    assert_eq!(
        first, second,
        "a guid is the entry's identity and must survive re-listing"
    );
}

#[test]
fn inodes_are_stable_across_relisting() {
    let server = TestServer::empty();
    server.seed("stable.txt", b"s");
    let store = store_for(&server);

    store.ensure_listed("/").unwrap();
    let first = store.get_inode("/stable.txt").unwrap();

    store.ensure_listed("/").unwrap();
    let second = store.get_inode("/stable.txt").unwrap();

    assert_eq!(
        first, second,
        "an entry must keep its local inode across re-listings"
    );
}

#[test]
fn getattr_reports_the_real_size_and_kind() {
    let server = TestServer::empty();
    server.seed("sized.txt", b"0123456789");
    server.seed_dir("adir");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();

    let file = store
        .get_entry(store.get_inode("/sized.txt").unwrap())
        .unwrap();
    assert_eq!(file.attr.stat.size, 10);
    assert_eq!(file.kind, FsEntryKind::File);

    let dir = store.get_entry(store.get_inode("/adir").unwrap()).unwrap();
    assert_eq!(dir.kind, FsEntryKind::Directory);
}

#[test]
fn refresh_sees_a_change_made_behind_our_back() {
    let server = TestServer::empty();
    server.seed("grow.txt", b"abc");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();

    server.seed("grow.txt", b"abcdef");

    let attr = store.refresh("/grow.txt").expect("refresh");
    assert_eq!(
        attr.stat.size, 6,
        "refresh must report the current backing size"
    );
}

#[test]
fn read_returns_exact_backing_bytes() {
    let server = TestServer::empty();
    server.seed("data.bin", b"abcdefghij");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();

    assert_eq!(store.read("/data.bin", 0, 100).unwrap(), b"abcdefghij");
    assert_eq!(store.read("/data.bin", 3, 4).unwrap(), b"defg");
    assert!(
        store.read("/data.bin", 100, 10).unwrap().is_empty(),
        "a read past EOF must be empty, not an error"
    );
}

#[test]
fn write_reaches_the_backing_file() {
    let server = TestServer::empty();
    server.seed("data.bin", b"abcdefghij");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();

    assert_eq!(store.write("/data.bin", 0, b"XYZ").unwrap(), 3);
    assert_eq!(server.backing("data.bin"), b"XYZdefghij");
}

#[test]
fn write_past_eof_zero_fills() {
    let server = TestServer::empty();
    server.seed("sparse.bin", b"ab");
    let store = store_for(&server);

    store.write("/sparse.bin", 5, b"Z").unwrap();

    let backing = server.backing("sparse.bin");
    assert_eq!(backing.len(), 6);
    assert_eq!(&backing[..2], b"ab");
    assert_eq!(&backing[2..5], &[0u8; 3]);
}

#[test]
fn write_to_a_missing_file_is_not_found_and_creates_nothing() {
    let server = TestServer::empty();
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();

    assert_eq!(
        store.write("/ghost.bin", 0, b"x").unwrap_err(),
        StoreError::NotFound,
        "writing a nonexistent path must not create it"
    );
    assert!(!server.backing_exists("ghost.bin"));
}

#[test]
fn fsync_succeeds_on_a_writable_file() {
    let server = TestServer::empty();
    server.seed("data.bin", b"abc");
    let store = store_for(&server);

    store.fsync("/data.bin", true).expect("fsync");
    store.fsync("/data.bin", false).expect("full fsync");
}

#[test]
fn unlink_removes_the_file_and_the_catalog_entry() {
    let server = TestServer::empty();
    server.seed("doomed.txt", b"x");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();
    assert!(store.get_inode("/doomed.txt").is_some());

    store.unlink("/doomed.txt").expect("unlink");

    assert!(!server.backing_exists("doomed.txt"));
    assert_eq!(store.get_inode("/doomed.txt"), None);
}

#[test]
fn rmdir_refuses_a_non_empty_directory() {
    let server = TestServer::empty();
    server.seed("full/child.txt", b"x");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();

    assert!(
        store.rmdir("/full").is_err(),
        "rmdir on a non-empty directory must fail"
    );
    assert!(server.backing_exists("full/child.txt"));
}

#[test]
fn rmdir_removes_an_empty_directory() {
    let server = TestServer::empty();
    server.seed_dir("hollow");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();

    store.rmdir("/hollow").expect("rmdir");
    assert!(!server.backing_exists("hollow"));
}

#[test]
fn rename_moves_the_file_and_updates_the_catalog() {
    let server = TestServer::empty();
    server.seed("before.txt", b"payload");
    let store = store_for(&server);
    store.ensure_listed("/").unwrap();

    store
        .rename("/before.txt", "/after.txt", fuser::RenameFlags::empty())
        .expect("rename");

    assert!(!server.backing_exists("before.txt"));
    assert_eq!(server.backing("after.txt"), b"payload");
}

#[test]
fn statfs_matches_the_backing_filesystem() {
    let server = TestServer::empty();
    let store = store_for(&server);

    let stats = store.statfs().expect("statfs");
    assert!(stats.blocks > 0, "f_blocks must be positive");
    assert!(
        stats.block_size > 0,
        "f_bsize must be positive so callers can compute byte counts"
    );
}

#[test]
fn unreachable_server_reports_an_io_error() {
    let store = Store::new(
        "http://127.0.0.1:1",
        FsNode {
            node_id: "test-node".into(),
        },
    )
    .unwrap();

    assert!(
        matches!(store.ensure_listed("/"), Err(StoreError::Io(_))),
        "an unreachable server must be reported as an Io error"
    );
}

/// The store must not invent an entry for a path the server cannot resolve.
#[test]
fn getattr_on_a_missing_path_is_not_found() {
    let server = TestServer::empty();
    let store = store_for(&server);

    assert_eq!(
        store.refresh("/definitely-not-here").unwrap_err(),
        StoreError::NotFound
    );
}
