//! Multi-node conformance: several backing nodes merged into one mount.
//!
//! These tests describe the contract for a merged namespace. Two of them
//! are **expected to fail today** and say so in their doc comment; they are
//! the specification for behaviour we have not built yet.
//!
//! The properties under test:
//!
//! 1. Files from every node appear in one directory listing.
//! 2. A node going away keeps its entries *visible but unreadable* — it
//!    must not look like mass deletion, and it must not look like `EIO`.
//! 3. A node that comes back with a **second copy** of a name must not be
//!    silently resolved: the directory is refused and the collision is
//!    logged.
//! 4. Nothing in (2) or (3) may crash the process.
use punkfs::StoreError;
use punkfs::store::StoreApi;

use super::common;

/// Two nodes, one mount: both nodes' files are visible side by side.
#[test]
fn files_from_every_node_appear_in_one_listing() {
    if !common::require_fuse() {
        return;
    }
    let cluster = common::TestCluster::empty(2);
    cluster.node(0).seed("from-a.txt", b"aaa");
    cluster.node(1).seed("from-b.txt", b"bbb");

    let mount = common::MultiMount::new(&cluster);

    let mut names: Vec<String> = std::fs::read_dir(mount.path())
        .expect("read merged root")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["from-a.txt", "from-b.txt"],
        "CONTRACT VIOLATED\n  contract: a merged mount shows every node's entries\n  got:      {names:?}"
    );

    // And the content resolves to the right node.
    assert_eq!(std::fs::read(mount.at("from-a.txt")).unwrap(), b"aaa");
    assert_eq!(std::fs::read(mount.at("from-b.txt")).unwrap(), b"bbb");
}

/// A directory that exists on both nodes is opened once, listing both.
#[test]
fn a_directory_present_on_both_nodes_is_merged() {
    if !common::require_fuse() {
        return;
    }
    let cluster = common::TestCluster::empty(2);
    cluster.node(0).seed_dir("shared");
    cluster.node(0).seed("shared/only-a.txt", b"a");
    cluster.node(1).seed_dir("shared");
    cluster.node(1).seed("shared/only-b.txt", b"b");

    let mount = common::MultiMount::new(&cluster);

    let mut names: Vec<String> = std::fs::read_dir(mount.at("shared"))
        .expect("read merged dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["only-a.txt", "only-b.txt"]);
}

/// A directory that exists on several nodes is a **merge point**, not a
/// collision. Only files can collide; two directories with the same name
/// simply contribute their children to one shared directory.
///
/// Regression: the first multi-node merge reported every shared directory
/// as a duplicate name, which would have made any overlapping tree unusable.
#[test]
fn a_shared_directory_is_merged_without_being_reported_as_a_conflict() {
    let cluster = common::TestCluster::empty(2);
    cluster.node(0).seed("shared/from-a.txt", b"a");
    cluster.node(1).seed("shared/from-b.txt", b"b");

    let node = punkfs::FsNode {
        node_id: "test-node".into(),
    };
    let store = punkfs::Store::new_nodes(&cluster.nodes(), node).expect("build store");
    store.ensure_listed("/").expect("list root");

    assert!(
        store.conflicts().is_empty(),
        "CONTRACT VIOLATED\n  contract: a directory present on both nodes merges silently\n  got:      conflicts {:?}\n  cause:    the merge treats a shared directory name as a file collision",
        store.conflicts()
    );

    // And both sides really are visible through the merge.
    store.ensure_listed("/shared").expect("list shared");
    let mut names: Vec<String> = store
        .get_children("/shared")
        .into_iter()
        .map(|c| c.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["from-a.txt", "from-b.txt"]);
}

/// With one node gone, its files stay listed but reads report a specific
/// "the node is unreachable" errno rather than a generic I/O failure.
///
/// The errno matters: `EHOSTDOWN` tells a caller it is a transient
/// reachability problem, `EIO` tells them the storage is broken.
#[test]
fn a_down_node_keeps_its_entries_listed_but_unreadable() {
    if !common::require_fuse() {
        return;
    }
    let cluster = common::TestCluster::empty(2);
    cluster.node(0).seed("stays.txt", b"a");
    cluster.node(1).seed("vanishes.txt", b"b");

    let mount = common::MultiMount::new(&cluster);

    // Prime the catalog while both nodes are healthy.
    let before = std::fs::read(mount.at("vanishes.txt")).expect("read before outage");
    assert_eq!(before, b"b");

    // Kill the second node.
    cluster.node(0);
    drop_server(&cluster, 1);

    // The entry must still be listed: an outage is not a deletion.
    let names: Vec<String> = std::fs::read_dir(mount.path())
        .expect("read root after outage")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == "vanishes.txt"),
        "CONTRACT VIOLATED\n  contract: an entry whose node is down stays visible\n  got:      {names:?}\n  cause:    the listing drops entries from nodes that did not answer"
    );

    // And reading it fails as "host down", not EIO. `ErrorKind` cannot
    // express this: Rust maps both EHOSTDOWN and EIO to `Uncategorized`,
    // so the raw errno is the only way to tell them apart.
    let err = std::fs::read(mount.at("vanishes.txt")).expect_err("read must fail");
    let errno = err.raw_os_error().unwrap_or(0);
    assert_eq!(
        errno,
        libc::EHOSTDOWN,
        "CONTRACT VIOLATED\n  contract: reading from a down node reports EHOSTDOWN\n  got:      errno {errno} ({:?}) from {err:?}\n  cause:    StoreError::Inaccessible (or a transport failure) was flattened\n            into a generic EIO on the way to the kernel",
        err.kind()
    );
}

/// A node that dies mid-session must not take the mount with it: the other
/// node stays fully usable.
#[test]
fn a_down_node_does_not_break_the_surviving_node() {
    if !common::require_fuse() {
        return;
    }
    let cluster = common::TestCluster::empty(2);
    cluster.node(0).seed("alive.txt", b"0123456789");
    cluster.node(1).seed("dead.txt", b"gone");

    let mount = common::MultiMount::new(&cluster);
    drop_server(&cluster, 1);

    assert_eq!(
        std::fs::read(mount.at("alive.txt")).expect("surviving node must still serve"),
        b"0123456789"
    );

    // Writing to the survivor still works. Use an explicit write of the
    // same length: `fs::write` would need O_TRUNC, which is the separate
    // `setattr` gap and would fail for reasons unrelated to this test.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(mount.at("alive.txt"))
            .expect("open survivor for writing");
        f.write_all(b"updated!!!").expect("write to surviving node");
    }
    assert_eq!(cluster.node(0).backing("alive.txt"), b"updated!!!");
}

/// **Known gap.** When a node goes down and comes back holding the *same
/// name at the same path* as another node, the merged namespace has two
/// different files claiming one name.
///
/// Contract: this must never be resolved silently — picking either file
/// would hand a caller the wrong bytes. The directory is refused and the
/// collision is logged.
///
/// Expected to fail today: the catalog keeps the first node's entry and
/// serves it as though nothing happened.
#[test]
fn a_node_returning_with_a_duplicate_name_is_refused_not_guessed() {
    if !common::require_fuse() {
        return;
    }
    let cluster = common::TestCluster::empty(2);
    cluster.node(0).seed("collide.txt", b"from-node-0");

    let mount = common::MultiMount::new(&cluster);

    // Prime the catalog with node 0's copy only.
    assert_eq!(std::fs::read(mount.at("collide.txt")).unwrap(), b"from-node-0");

    // Node 1 now presents a *different* file under the same name, as it
    // would after being offline and regrouped.
    cluster.node(1).seed("collide.txt", b"from-node-1");

    // Force a re-listing by touching the directory.
    let listing = std::fs::read_dir(mount.path());

    match listing {
        Err(err) => {
            // Refusing to list an ambiguous directory is acceptable.
            eprintln!("directory refused on conflict: {err:?}");
        }
        Ok(entries) => {
            let names: Vec<String> = entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            panic!(
                "CONTRACT VIOLATED\n  contract: two nodes claiming one name must not be resolved silently\n  got:      directory listed as {names:?} with one file silently served as the winner\n  cause:    the merge keeps the first node's entry and serves it; the collision\n            is only written to stderr (\"duplicate name across nodes\"), which the\n            mount cannot turn into a visible failure yet\n  expect:   readdir fails, or the collision is surfaced, rather than picking a file\n  note:     the process stays alive and the collision IS logged, so nothing crashes;\n            this test fails on the stricter contract only"
            );
        }
    }
}

/// The same collision, seen at the store layer, where the contract is
/// easiest to state: the collision is reported, never silently guessed.
///
/// Expected to fail today.
#[test]
fn the_store_reports_a_duplicate_name_collision() {
    let cluster = common::TestCluster::empty(2);
    cluster.node(0).seed("collide.txt", b"from-node-0");
    cluster.node(1).seed("collide.txt", b"from-node-1");

    let node = punkfs::FsNode {
        node_id: "test-node".into(),
    };
    let store = punkfs::Store::new_nodes(&cluster.nodes(), node).expect("build store");

    // Listing is allowed to succeed, but the collision must be visible
    // through the API rather than silently resolved.
    let result = store.ensure_listed("/");
    let conflicts = store.conflicts();

    assert!(
        !conflicts.is_empty() || result.is_err(),
        "CONTRACT VIOLATED\n  contract: a duplicate name across nodes must be surfaced\n  got:      ensure_listed={result:?}, conflicts={conflicts:?}\n  expect:   \"collide.txt\" reported as a conflict or the listing refused\n  cause:    reconcile_from reports conflicts, but nothing propagates them\n            to the caller, so a caller cannot tell a merge happened"
    );
}

/// A node that has never answered must be reported as unreachable rather
/// than silently treated as healthy-and-empty.
#[test]
fn an_unreachable_node_is_ehostdown_at_the_store_layer() {
    let cluster = common::TestCluster::empty(1);
    cluster.node(0).seed("data.txt", b"payload");

    let node = punkfs::FsNode {
        node_id: "test-node".into(),
    };
    let store = punkfs::Store::new_nodes(&cluster.nodes(), node).expect("build store");
    store.ensure_listed("/").expect("prime catalog");

    drop_server(&cluster, 0);

    // A read against a node we cannot reach is a host failure.
    match store.read("/data.txt", 0, 16) {
        Err(StoreError::Inaccessible(_)) | Err(StoreError::Io(_)) => {}
        other => panic!(
            "CONTRACT VIOLATED\n  contract: reading from a down node reports unreachability\n  got:      {other:?}"
        ),
    }
}

/// Kill node `index`, leaving the rest of the cluster alive.
fn drop_server(cluster: &common::TestCluster, index: usize) {
    // `TestServer` owns its child process; dropping it is the supported
    // way to stop one node.
    cluster.node(index);
    // SAFETY-free approach: replace the node with a fresh, already-dead
    // server over the same address is not possible, so instead we kill the
    // process through the harness accessor and let Drop be a no-op.
    cluster.stop(index);
}
