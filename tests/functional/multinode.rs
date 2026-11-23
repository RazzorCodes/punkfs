//! Multi-node behaviour seen at the store layer: the client `Store` merged
//! over several live `punkfs-server` processes, with no FUSE mount in the
//! way. The whole operation — build a store over N nodes, list, reconcile,
//! read — is exercised and its outcome asserted.

use punkfs::StoreError;
use punkfs::store::StoreApi;

use crate::common::TestCluster;

/// A directory present on two nodes is a **merge point**, not a collision.
/// Only files can collide; two directories with the same name simply
/// contribute their children to one shared directory.
///
/// Regression: the first multi-node merge reported every shared directory
/// as a duplicate name, which would have made any overlapping tree unusable.
#[test]
fn functional_shared_directory_merges_at_the_store_layer() {
    let cluster = TestCluster::empty(2);
    cluster.node(0).seed("shared/from-a.txt", b"a");
    cluster.node(1).seed("shared/from-b.txt", b"b");

    let node = punkfs::FsNode {
        node_id: "test-node".into(),
    };
    let store = punkfs::Store::new_nodes(&cluster.nodes(), node).expect("build store");
    store.ensure_listed("/").expect("list root");

    assert!(
        store.conflicts().is_empty(),
        "a directory present on both nodes must merge silently, got conflicts {:?}",
        store.conflicts()
    );

    store.ensure_listed("/shared").expect("list shared");
    let mut names: Vec<String> = store
        .get_children("/shared")
        .into_iter()
        .map(|c| c.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["from-a.txt", "from-b.txt"]);
}

/// The same file name present on two nodes is a collision, and it is
/// reported through the API rather than silently resolved.
#[test]
fn functional_store_reports_a_duplicate_name_collision() {
    let cluster = TestCluster::empty(2);
    cluster.node(0).seed("collide.txt", b"from-node-0");
    cluster.node(1).seed("collide.txt", b"from-node-1");

    let node = punkfs::FsNode {
        node_id: "test-node".into(),
    };
    let store = punkfs::Store::new_nodes(&cluster.nodes(), node).expect("build store");

    let result = store.ensure_listed("/");
    let conflicts = store.conflicts();

    assert!(
        !conflicts.is_empty() || result.is_err(),
        "a duplicate name across nodes must be surfaced: ensure_listed={result:?}, conflicts={conflicts:?}"
    );
}

/// A node that has never answered must be reported as unreachable rather
/// than silently treated as healthy-and-empty.
#[test]
fn functional_store_reports_unreachable_node_as_host_down() {
    let cluster = TestCluster::empty(1);
    cluster.node(0).seed("data.txt", b"payload");

    let node = punkfs::FsNode {
        node_id: "test-node".into(),
    };
    let store = punkfs::Store::new_nodes(&cluster.nodes(), node).expect("build store");
    store.ensure_listed("/").expect("prime catalog");

    cluster.stop(0);

    match store.read("/data.txt", 0, 16) {
        Err(StoreError::Inaccessible(_)) | Err(StoreError::Io(_)) => {}
        other => panic!(
            "reading from a down node must report unreachability, got {other:?}"
        ),
    }
}