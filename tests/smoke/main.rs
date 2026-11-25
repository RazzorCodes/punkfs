//! Smoke tests: fast, in-process, one behavior per test (one function or
//! one function branch per assertion). No FUSE, no network, no subprocess.
//! These pin the public building blocks the functional and integration
//! suites build on — if a merge or liveness primitive is wrong, the larger
//! tests that assume it would tell a confusing story, so we pin the
//! primitive cheaply here.

mod posix_backing;

use std::time::{Duration, SystemTime};

use punkfs::core::cache::metadata::{FsNode, ListingOutcome, MetadataCache, NodeStatus};
use punkfs::core::metadata::{Availability, FsEntry};
use punkfs::core::types::{FileAttrData, FileTimes, FsEntryKind, Stat};

// ---------------------------------------------------------------------------
// Catalog identity primitives (public meat of the merge engine).
// ---------------------------------------------------------------------------

/// An entry knows the node that holds its bytes and whether that node is
/// answering; those two fields are what drive routing and outage behaviour.
#[test]
fn smoke_entry_carries_owner_and_availability() {
    let mut cache = MetadataCache::new();
    let e = FsEntry {
        inode: 0,
        path: "/f".into(),
        guid: "g".into(),
        kind: FsEntryKind::File,
        node_id: "node-a".into(),
        availability: Availability::Available,
        attr: FileAttrData {
            guid: "g".into(),
            kind: FsEntryKind::File,
            ..Default::default()
        },
    };
    assert_eq!(e.node_id, "node-a");
    assert_eq!(e.availability, Availability::Available);

    // The catalog really does answer lookups by path and by inode.
    cache.upsert_entry(e.clone());
    let inode = cache.get_inode("/f").unwrap();
    assert_eq!(cache.get_path(inode), Some("/f"));
}

/// `get_children_of` returns only direct descendants: a sibling prefix must
/// not be reported as a child.
#[test]
fn smoke_children_are_direct_descendants_only() {
    let mut cache = MetadataCache::new();
    cache.insert_root("/", "root", FsEntryKind::Directory);
    cache.upsert_entry(file("/docs/a", "a"));
    cache.upsert_entry(file("/docsx", "x"));
    let paths: Vec<String> = cache
        .get_children_of("/docs")
        .into_iter()
        .map(|e| e.path.clone())
        .collect();
    assert_eq!(paths, vec!["/docs/a"]);
    assert!(
        cache
            .get_children_of("/docs")
            .iter()
            .all(|e| e.path != "/docsx")
    );
}

/// A removed entry (and, for a directory, its whole subtree) leaves the
/// catalog, while an untouched sibling survives.
#[test]
fn smoke_remove_drops_the_subtree_only() {
    let mut cache = MetadataCache::new();
    cache.insert_root("/", "root", FsEntryKind::Directory);
    cache.upsert_entry(dir("/dir", "dir"));
    cache.upsert_entry(file("/dir/a", "a"));
    cache.upsert_entry(file("/dir/sub/b", "b"));
    cache.upsert_entry(file("/other.c", "other"));

    cache.remove_entry("/dir");

    assert!(cache.get_inode("/dir").is_none());
    assert!(cache.get_inode("/dir/a").is_none());
    assert!(cache.get_inode("/dir/sub/b").is_none());
    assert!(cache.get_inode("/other.c").is_some());
    assert!(cache.get_inode("/").is_some(), "root is untouched");
}

#[test]
fn smoke_exchange_swaps_cached_subtrees_and_keeps_inodes() {
    let mut cache = MetadataCache::new();
    cache.insert_root("/", "root", FsEntryKind::Directory);
    cache.upsert_entry(dir("/docs", "docs"));
    cache.upsert_entry(dir("/docs/sub", "sub"));
    cache.upsert_entry(file("/docs/sub/b", "b"));
    cache.upsert_entry(dir("/other", "other"));
    cache.upsert_entry(file("/other/file", "other-file"));
    let docs = cache.get_inode("/docs").unwrap();
    let nested = cache.get_inode("/docs/sub/b").unwrap();
    let other = cache.get_inode("/other").unwrap();
    let other_file = cache.get_inode("/other/file").unwrap();
    cache.exchange_entries("/docs", "/other");
    assert_eq!(cache.get_inode("/other"), Some(docs));
    assert_eq!(cache.get_inode("/other/sub/b"), Some(nested));
    assert_eq!(cache.get_inode("/docs"), Some(other));
    assert_eq!(cache.get_inode("/docs/file"), Some(other_file));
}

/// `FsEntryKind` maps to the attr shapes: a directory vs file vs link are
/// distinct kinds that the attr translation later turns into kernel types.
#[test]
fn smoke_entry_kinds_are_distinct() {
    assert_ne!(FsEntryKind::Directory, FsEntryKind::File);
    assert_ne!(FsEntryKind::File, FsEntryKind::Link);
    assert_eq!(FsEntryKind::default(), FsEntryKind::Unknown);
}

/// A granted inode is never reused, so a retired inode cannot silently
/// alias a different file.
#[test]
fn smoke_allocated_inodes_are_not_reused() {
    let mut cache = MetadataCache::new();
    cache.insert_root("/", "root", FsEntryKind::Directory);
    cache.upsert_entry(file("/a", "a"));
    let a = cache.get_inode("/a").unwrap();
    cache.remove_entry("/a");
    cache.upsert_entry(file("/a", "a2"));
    assert_ne!(cache.get_inode("/a").unwrap(), a);
}

// ---------------------------------------------------------------------------
// Reconciliation outcome branches.
// ---------------------------------------------------------------------------

/// A node that answers drops its own un-reported entries but never another
/// node's: one node's listing cannot delete a sibling's files.
#[test]
fn smoke_answered_node_only_drops_its_own_stale_entries() {
    let mut cache = MetadataCache::new();
    cache.insert_root("/", "root", FsEntryKind::Directory);
    cache.reconcile_from(
        "/",
        vec![
            (
                "node-a".to_owned(),
                ListingOutcome::Answered(vec![owned("/a.txt", "node-a")]),
            ),
            (
                "node-b".to_owned(),
                ListingOutcome::Answered(vec![owned("/b.txt", "node-b")]),
            ),
        ],
    );

    // node-b answers with nothing now: only node-b's own entry may drop.
    cache.reconcile_from(
        "/",
        vec![("node-b".to_owned(), ListingOutcome::Answered(vec![]))],
    );
    assert!(
        cache.get_inode("/b.txt").is_none(),
        "node-b's stale entry is gone"
    );
    assert!(
        cache.get_inode("/a.txt").is_some(),
        "node-a's entry must survive node-b's listing"
    );
}

/// An unreachable node's entries are kept and marked impossible to read, so
/// an outage is never presented as deletion.
#[test]
fn smoke_unreachable_marks_cached_entries_inaccessible() {
    let mut cache = MetadataCache::new();
    cache.insert_root("/", "root", FsEntryKind::Directory);
    cache.upsert_entry(owned("/a.txt", "node-a"));
    cache.reconcile_from(
        "/",
        vec![("node-a".to_owned(), ListingOutcome::Unreachable)],
    );
    let kept = cache.get_entry_by_path("/a.txt").expect("kept");
    assert_eq!(kept.availability, Availability::Inaccessible);
}

// ---------------------------------------------------------------------------
// Liveness + node identity primitives.
// ---------------------------------------------------------------------------

/// A node id is validated as non-empty by the lifecycle layer; the catalog
/// simply records whatever identity is bound to an entry.
#[test]
fn smoke_node_liveness_is_time_bounded() {
    let mut status = NodeStatus::new("node-a");
    let now = SystemTime::now();
    assert!(!status.is_reachable(now, Duration::from_secs(5)));
    status.mark_seen(now);
    assert!(status.is_reachable(now, Duration::from_secs(5)));
    assert!(!status.is_reachable(now + Duration::from_secs(10), Duration::from_secs(5)));
}

/// `FsNode` is the machine identity used to tag locally-created entries.
#[test]
fn smoke_fsnode_is_a_labelled_identity() {
    let node = FsNode {
        node_id: "my-machine".into(),
    };
    assert_eq!(node.node_id, "my-machine");
}

// ---------------------------------------------------------------------------
// Attribute value primitives.
// ---------------------------------------------------------------------------

/// Stored stat defaults must be usable for callers computing block counts.
#[test]
fn smoke_stat_defaults_have_a_usable_block_size() {
    assert_eq!(Stat::default().block_size, 512);
}

/// Timestamps round-trip through whole-second values, the resolution the
/// wire protocol carries.
#[test]
fn smoke_file_times_convert_from_seconds() {
    let times = FileTimes::from_secs(1, 2, 3, 4);
    assert_eq!(
        times.accessed,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1)
    );
    assert_eq!(
        times.created,
        SystemTime::UNIX_EPOCH + Duration::from_secs(4)
    );
}

#[test]
fn smoke_toml_config_parses_peers_and_cache_limits() {
    let parsed = punkfs::core::config::NodeTomlConfig::parse_toml(
        r#"
mountpoint = "/tmp/mnt"
node_id = "node-a"
listen = "127.0.0.1:9100"
db_path = "state/node-a.redb"
cache_ttl_ms = 2500
cache_max_entries = 128

[peers]
node-b = "http://127.0.0.1:9101"
"#,
        Some(std::path::Path::new("/etc/punkfs/node-a.toml")),
    )
    .unwrap();
    assert_eq!(parsed.node_id.as_deref(), Some("node-a"));
    assert_eq!(
        parsed.db_path.as_deref(),
        Some(std::path::Path::new("/etc/punkfs/state/node-a.redb"))
    );
    assert_eq!(parsed.cache_ttl, Duration::from_millis(2500));
    assert_eq!(parsed.cache_max_entries, 128);
    assert_eq!(
        parsed.peers,
        vec![("node-b".into(), "http://127.0.0.1:9101".into())]
    );
}

#[test]
fn smoke_redb_persists_constant_inodes_across_reopen_and_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("node.redb");
    let db = std::sync::Arc::new(punkfs::db::persistent::PersistentDb::open(&db_path).unwrap());
    let mut cache = MetadataCache::new();
    cache.attach_db(db, None, Some(2)).unwrap();
    cache.insert_root("/", "/", FsEntryKind::Directory);
    cache.upsert_entry(file("/a.txt", "ga"));
    cache.upsert_entry(file("/b.txt", "gb"));
    let ino_a = cache.lookup_inode("/a.txt").unwrap();
    let ino_b = cache.lookup_inode("/b.txt").unwrap();
    assert_ne!(ino_a, ino_b);

    // Even if evicted from the bounded in-memory cache, lookup still hits redb.
    assert!(cache.entries.len() <= 2);
    assert_eq!(cache.lookup_inode("/a.txt"), Some(ino_a));
    assert_eq!(cache.lookup_path(ino_a).as_deref(), Some("/a.txt"));

    drop(cache);
    let reopened =
        std::sync::Arc::new(punkfs::db::persistent::PersistentDb::open(&db_path).unwrap());
    let mut cache2 = MetadataCache::new();
    cache2.attach_db(reopened, None, None).unwrap();
    assert_eq!(cache2.lookup_inode("/a.txt"), Some(ino_a));
    assert_eq!(cache2.lookup_inode("/b.txt"), Some(ino_b));
}

#[test]
fn smoke_persisted_snapshot_preserves_renamed_inode_and_reconciles_directories() {
    use punkfs::db::persistent::PersistentDb;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("metadata.redb");
    let db = PersistentDb::open(&path).unwrap();
    let before = file("/before", "stable");
    let old_dir = dir("/gone", "dir-guid");
    let (initial, _) = db
        .reconcile_owner_snapshot("mock", &[before, old_dir])
        .unwrap();
    let inode = initial.iter().find(|e| e.path == "/before").unwrap().inode;
    drop(db);
    let db = PersistentDb::open(&path).unwrap();
    db.reconcile_owner_snapshot("mock", &[file("/after", "stable")])
        .unwrap();
    assert_eq!(db.get_inode("/after").unwrap(), Some(inode));
    assert_eq!(db.get_path(inode).unwrap().as_deref(), Some("/after"));
    assert!(db.get_inode("/before").unwrap().is_none());
    assert!(db.get_inode("/gone").unwrap().is_none());
    db.reconcile_owner_snapshot("mock", &[file("/after", "replacement-guid")])
        .unwrap();
    assert_ne!(db.get_inode("/after").unwrap(), Some(inode));
    assert_eq!(db.get_path(inode).unwrap(), None);
}

#[test]
fn smoke_removing_one_directory_owner_preserves_other_contributions() {
    use punkfs::db::persistent::PersistentDb;
    let temp = tempfile::tempdir().unwrap();
    let db = PersistentDb::open(&temp.path().join("metadata.redb")).unwrap();
    let mut a_dir = dir("/shared", "dir-a");
    a_dir.node_id = "a".into();
    let mut b_dir = dir("/shared", "dir-b");
    b_dir.node_id = "b".into();
    let mut b_file = file("/shared/b", "file-b");
    b_file.node_id = "b".into();
    db.reconcile_owner_snapshot("a", &[a_dir]).unwrap();
    let inode = db.get_inode("/shared").unwrap();
    db.reconcile_owner_snapshot("b", &[b_dir, b_file]).unwrap();
    db.reconcile_owner_snapshot("a", &[]).unwrap();
    assert_eq!(db.get_inode("/shared").unwrap(), inode);
    assert_eq!(db.get_by_path("/shared").unwrap().unwrap().node_id, "b");
    assert_eq!(db.get_by_path("/shared/b").unwrap().unwrap().node_id, "b");
}

// Helpers -------------------------------------------------------------------

fn file(path: &str, guid: &str) -> FsEntry {
    FsEntry {
        inode: 0,
        path: path.into(),
        guid: guid.into(),
        kind: FsEntryKind::File,
        node_id: "mock".into(),
        availability: Availability::Available,
        attr: FileAttrData {
            guid: guid.into(),
            kind: FsEntryKind::File,
            ..Default::default()
        },
    }
}

fn dir(path: &str, guid: &str) -> FsEntry {
    let mut e = file(path, guid);
    e.kind = FsEntryKind::Directory;
    e
}

fn owned(path: &str, node: &str) -> FsEntry {
    let mut e = file(path, &format!("guid-{node}"));
    e.node_id = node.into();
    e
}
