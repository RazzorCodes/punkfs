//! Functional tests: exercise a whole operation end to end and assert the
//! *desired* outcome — not argument forwarding. These drive the public
//! `MetadataCache` merge engine in-process (each backing node feeds a
//! listing outcome), so no FUSE, network, or subprocess is needed: the
//! catalog is the seam the real store feeds through the same protocol the
//! gRPC layer uses. The mock "system" is the set of `ListingOutcome`s each
//! node contributes, kept to exactly what the assertion cares about.
//!
//! The endgame behaviour under test: one folder is assembled from several
//! remote backing folders, and a change on one backing folder is present
//! and viewable in the merged view. Tests that assert this and fail are
//! allowed to stay failing; we build the implementation toward them.

use std::time::{Duration, SystemTime};

pub use punkfs::core::cache::metadata::{ListingOutcome, MetadataCache};
pub use punkfs::core::metadata::{Availability, FsEntry};
use punkfs::core::types::{FileAttrData, FsEntryKind};

/// Build a catalog entry owned by `node`.
fn entry(node: &str, path: &str, guid: &str, kind: FsEntryKind) -> FsEntry {
    FsEntry {
        inode: 0, // catalog allocates
        path: path.to_owned(),
        guid: guid.to_owned(),
        kind,
        node_id: node.to_owned(),
        availability: Availability::Available,
        attr: FileAttrData {
            guid: guid.to_owned(),
            kind,
            ..FileAttrData::default()
        },
    }
}

fn dir(node: &str, path: &str, guid: &str) -> FsEntry {
    entry(node, path, guid, FsEntryKind::Directory)
}
fn file(node: &str, path: &str, guid: &str) -> FsEntry {
    entry(node, path, guid, FsEntryKind::File)
}

/// A fresh catalog rooted at `/`.
fn catalog() -> MetadataCache {
    let mut cache = MetadataCache::new();
    cache.insert_root("/", "root-guid", FsEntryKind::Directory);
    cache
}

/// The children of `dir` as sorted paths.
fn child_paths(cache: &MetadataCache, dir: &str) -> Vec<String> {
    let mut paths: Vec<String> = cache
        .get_children_of(dir)
        .into_iter()
        .map(|e| e.path.clone())
        .collect();
    paths.sort();
    paths
}

// ---------------------------------------------------------------------------
// Multi-node merge: the endgame behaviour.
// ---------------------------------------------------------------------------

/// The whole point: a folder assembled from two backing folders shows the
/// union of both, with each child mapped to its owning node.
#[test]
fn functional_merge_two_nodes_shows_the_union_with_owners() {
    let mut cache = catalog();
    cache.reconcile_from(
        "/",
        vec![
            (
                "node-a".to_owned(),
                ListingOutcome::Answered(vec![file("node-a", "/a.txt", "guid-a")]),
            ),
            (
                "node-b".to_owned(),
                ListingOutcome::Answered(vec![file("node-b", "/b.txt", "guid-b")]),
            ),
        ],
    );

    assert_eq!(child_paths(&cache, "/"), vec!["/a.txt", "/b.txt"]);

    let a = cache.get_entry_by_path("/a.txt").expect("a.txt is present");
    assert_eq!(a.node_id, "node-a", "entry must carry its owning node");
    let b = cache.get_entry_by_path("/b.txt").expect("b.txt is present");
    assert_eq!(b.node_id, "node-b");
}

/// A directory present on two nodes is a merge point, not a conflict, and
/// both sides' children survive so traversal is complete.
#[test]
fn functional_shared_directory_merges_both_sides() {
    let mut cache = catalog();
    cache.reconcile_from(
        "/",
        vec![
            (
                "node-a".to_owned(),
                ListingOutcome::Answered(vec![dir("node-a", "/shared", "guid-sa")]),
            ),
            (
                "node-b".to_owned(),
                ListingOutcome::Answered(vec![dir("node-b", "/shared", "guid-sb")]),
            ),
        ],
    );
    assert_eq!(
        child_paths(&cache, "/"),
        vec!["/shared"],
        "a shared directory appears exactly once"
    );

    cache.reconcile_from(
        "/shared",
        vec![
            (
                "node-a".to_owned(),
                ListingOutcome::Answered(vec![file("node-a", "/shared/from-a", "ga")]),
            ),
            (
                "node-b".to_owned(),
                ListingOutcome::Answered(vec![file("node-b", "/shared/from-b", "gb")]),
            ),
        ],
    );

    assert_eq!(
        child_paths(&cache, "/shared"),
        vec!["/shared/from-a", "/shared/from-b"]
    );
    assert!(
        cache
            .get_children_of("/")
            .iter()
            .filter(|e| e.path == "/shared")
            .count()
            == 1,
        "a merged directory must never be duplicated at its parent"
    );
}

/// A name served by two nodes as *files* is a genuine collision: it is
/// reported and a stable winner is chosen, never silently merged.
#[test]
fn functional_two_files_with_one_name_are_a_conflict_not_a_merge() {
    let mut cache = catalog();
    let conflicts = cache.reconcile_from(
        "/",
        vec![
            (
                "node-a".to_owned(),
                ListingOutcome::Answered(vec![file("node-a", "/dup.txt", "guid-a")]),
            ),
            (
                "node-b".to_owned(),
                ListingOutcome::Answered(vec![file("node-b", "/dup.txt", "guid-b")]),
            ),
        ],
    );

    assert_eq!(
        conflicts,
        vec!["/dup.txt"],
        "the collision must be reported"
    );
    assert_eq!(
        cache.get_entry_by_path("/dup.txt").unwrap().node_id,
        "node-a",
        "first node by precedence wins deterministically"
    );
}

/// A node going away keeps its entries browsable but marks them
/// inaccessible; a node coming back restores them. Browsing must never look
/// like a mass deletion during an outage.
#[test]
fn functional_outage_keeps_entries_and_recovery_restores_them() {
    let mut cache = catalog();
    cache.reconcile_from(
        "/",
        vec![(
            "node-a".to_owned(),
            ListingOutcome::Answered(vec![file("node-a", "/gone.txt", "guid-g")]),
        )],
    );
    assert_eq!(
        cache.get_entry_by_path("/gone.txt").unwrap().availability,
        Availability::Available
    );

    // The owning node itself goes quiet: its entry stays listed but is
    // marked unreadable rather than deleted.
    cache.reconcile_from(
        "/",
        vec![("node-a".to_owned(), ListingOutcome::Unreachable)],
    );
    let kept = cache
        .get_entry_by_path("/gone.txt")
        .expect("an outage must not delete the entry");
    assert_eq!(kept.availability, Availability::Inaccessible);

    cache.reconcile_from(
        "/",
        vec![(
            "node-a".to_owned(),
            ListingOutcome::Answered(vec![file("node-a", "/gone.txt", "guid-g")]),
        )],
    );
    assert_eq!(
        cache.get_entry_by_path("/gone.txt").unwrap().availability,
        Availability::Available,
        "when the node returns the entry becomes readable again"
    );
}

/// A change on one backing node — the same guid re-reported with a new
/// size — is visible in the merged view under the same inode.
#[test]
fn functional_a_change_on_one_node_is_visible_in_the_merged_view() {
    let mut cache = catalog();
    cache.reconcile_from(
        "/",
        vec![(
            "node-a".to_owned(),
            ListingOutcome::Answered(vec![{
                let mut e = file("node-a", "/grow.txt", "guid-g");
                e.attr.stat.size = 3;
                e
            }]),
        )],
    );
    let inode = cache.get_inode("/grow.txt").unwrap();
    assert_eq!(cache.get_entry(inode).unwrap().attr.stat.size, 3);

    // The backing node grows the file and re-lists; the merged view must
    // report the new size for the *same* inode.
    cache.reconcile_from(
        "/",
        vec![(
            "node-a".to_owned(),
            ListingOutcome::Answered(vec![{
                let mut e = file("node-a", "/grow.txt", "guid-g");
                e.attr.stat.size = 9;
                e
            }]),
        )],
    );
    assert_eq!(
        cache.get_inode("/grow.txt"),
        Some(inode),
        "a listing must not churn the inode the kernel already holds"
    );
    assert_eq!(cache.get_entry(inode).unwrap().attr.stat.size, 9);
}

// ---------------------------------------------------------------------------
// Local create + catalog wiring.
// ---------------------------------------------------------------------------

/// The full create flow: a new file is added to the catalog, the kernel is
/// handed a stable inode, and a rename keeps that identity while updating
/// the path the merged view shows.
#[test]
fn functional_create_then_rename_preserves_identity() {
    let mut cache = catalog();
    cache.upsert_entry(file("local-node", "/new.txt", "guid-new"));
    let created = cache.get_inode("/new.txt").expect("new file cached");
    assert_eq!(cache.get_path(created), Some("/new.txt"));

    cache.rename_entry("/new.txt", "/moved.txt");
    assert_eq!(
        cache.get_inode("/moved.txt"),
        Some(created),
        "rename must keep the same inode"
    );
    assert_eq!(cache.get_path(created), Some("/moved.txt"));
    assert_eq!(cache.get_inode("/new.txt"), None, "the old path is gone");
}

/// The catalog keeps exactly one entry per path: the EEXIST guard in the
/// passthrough create path rests on `get_inode` finding the already-known
/// name, so a second contributor for the same path must never create a
/// duplicate row the guard would not see.
#[test]
fn functional_create_sees_one_entry_per_path() {
    let mut cache = catalog();
    cache.reconcile_from(
        "/",
        vec![(
            "node-r".to_owned(),
            ListingOutcome::Answered(vec![file("node-r", "/reserved.txt", "guid-r")]),
        )],
    );
    // From a local create's point of view the name is already present, so
    // the guard route (`get_inode`) refuses before any upsert happens.
    assert!(cache.get_inode("/reserved.txt").is_some());

    // Even if a second listing reports the name again, the catalog keeps one
    // row per path and does not leak duplicate entries.
    cache.reconcile_from(
        "/",
        vec![(
            "node-r".to_owned(),
            ListingOutcome::Answered(vec![file("node-r", "/reserved.txt", "guid-r")]),
        )],
    );
    assert_eq!(cache.get_children_of("/").len(), 1);
}

// ---------------------------------------------------------------------------
// Node liveness bookkeeping.
// ---------------------------------------------------------------------------

/// Liveness drives the outage behaviour above: a node is reachable only
/// after it answers and only within the window.
#[test]
fn functional_node_liveness_tracks_recent_answers() {
    let mut status = punkfs::core::cache::metadata::NodeStatus::new("node-a");
    let now = SystemTime::now();
    assert!(
        !status.is_reachable(now, Duration::from_secs(5)),
        "never seen"
    );

    status.mark_seen(now);
    assert!(status.is_reachable(now, Duration::from_secs(5)));

    let later = now + Duration::from_secs(10);
    assert!(
        !status.is_reachable(later, Duration::from_secs(5)),
        "a node that fell silent is unreachable"
    );
}
