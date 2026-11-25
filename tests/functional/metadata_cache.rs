//! Exercise the public cache and attributes shared across entry kinds.
use std::time::{Duration, UNIX_EPOCH};

use punkfs::core::cache::metadata::{ListingOutcome, MetadataCache};
use punkfs::core::metadata::{Availability, FsEntry};
use punkfs::core::types::misc::{Access, FileAttrData, FileTimes, FsEntryKind, Stat};
use punkfs::telemetry::{self, Config, Destination, OperationContext, Severity};

#[test]
fn cache_keeps_extracted_attributes_and_inode_across_listings_and_outages() {
    let attributes = FileAttrData {
        guid: "file-guid".into(),
        kind: FsEntryKind::File,
        times: FileTimes::from_secs(1, 2, 3, 4),
        access: Access {
            mode: 0o640,
            uid: 12,
            gid: 34,
        },
        stat: Stat {
            size: 42,
            ..Stat::default()
        },
    };
    let entry = FsEntry {
        inode: 0,
        path: "/file".into(),
        guid: attributes.guid.clone(),
        kind: attributes.kind,
        node_id: "node-a".into(),
        availability: Availability::Available,
        attr: attributes.clone(),
    };
    let mut cache = MetadataCache::new();
    cache.insert_root("/", "root", FsEntryKind::Directory);
    assert!(
        cache
            .reconcile_from(
                "/",
                vec![(
                    "node-a".into(),
                    ListingOutcome::Answered(vec![entry.clone()])
                )]
            )
            .is_empty()
    );
    let inode = cache.get_inode("/file").unwrap();
    let cached = cache.get_entry(inode).unwrap();
    assert_eq!(cached.attr, attributes);
    assert_eq!(
        cached.attr.times.accessed,
        UNIX_EPOCH + Duration::from_secs(1)
    );
    assert_eq!(cached.attr.stat.block_size, 512);

    cache.reconcile_from("/", vec![("node-a".into(), ListingOutcome::Unreachable)]);
    let offline = cache.get_entry(inode).unwrap();
    assert_eq!(offline.availability, Availability::Inaccessible);
    assert_eq!(offline.attr, attributes);

    cache.reconcile_from(
        "/",
        vec![("node-a".into(), ListingOutcome::Answered(vec![entry]))],
    );
    assert_eq!(cache.get_inode("/file"), Some(inode));
    assert_eq!(
        cache.get_entry(inode).unwrap().availability,
        Availability::Available
    );
    assert_eq!(cache.get_entry(inode).unwrap().attr, attributes);
}

#[test]
fn shared_attributes_are_preserved_for_directories() {
    let attr = FileAttrData {
        guid: "directory-guid".into(),
        kind: FsEntryKind::Directory,
        times: FileTimes::from_secs(11, 12, 13, 14),
        access: Access {
            mode: 0o750,
            uid: 100,
            gid: 200,
        },
        stat: Stat {
            size: 4096,
            nlink: 2,
            ..Stat::default()
        },
    };
    let entry = FsEntry {
        inode: 0,
        path: "/dir".into(),
        guid: attr.guid.clone(),
        kind: attr.kind,
        node_id: "node-a".into(),
        availability: Availability::Available,
        attr: attr.clone(),
    };
    let mut cache = MetadataCache::new();
    cache.insert_root("/", "root", FsEntryKind::Directory);
    cache.reconcile_from(
        "/",
        vec![("node-a".into(), ListingOutcome::Answered(vec![entry]))],
    );
    let cached = cache.get_entry_by_path("/dir").unwrap();
    assert_eq!(cached.kind, FsEntryKind::Directory);
    assert_eq!(cached.attr, attr);
}

fn event_test_entry(path: &str, guid: &str, node: &str) -> FsEntry {
    FsEntry {
        inode: 0,
        path: path.into(),
        guid: guid.into(),
        kind: FsEntryKind::File,
        node_id: node.into(),
        availability: Availability::Available,
        attr: FileAttrData {
            guid: guid.into(),
            kind: FsEntryKind::File,
            ..FileAttrData::default()
        },
    }
}

fn events(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn logger_config(path: &std::path::Path, threshold: Severity) -> Config {
    Config {
        threshold,
        destination: Destination::File(path.into()),
        queue_capacity: 128,
        max_file_bytes: 100_000,
        retained_files: 1,
        service: "cache-test".into(),
    }
}

#[test]
fn cache_events_describe_conflicts_outages_recovery_and_mutations_without_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let (logger, mut guard) = telemetry::start(logger_config(&path, Severity::Trace)).unwrap();
    let mut cache = MetadataCache::with_logger(logger);
    cache.insert_root("/", "root", FsEntryKind::Directory);
    let secret_path = "/private-filename-must-not-be-logged";
    let entry = event_test_entry(secret_path, "private-guid-must-not-be-logged", "node-a");
    let conflict_ctx = OperationContext::new();
    let conflict = cache.reconcile_from_in(
        &conflict_ctx,
        "/",
        vec![
            (
                "node-a".into(),
                ListingOutcome::Answered(vec![entry.clone()]),
            ),
            (
                "node-b".into(),
                ListingOutcome::Answered(vec![event_test_entry(
                    secret_path,
                    "other-guid",
                    "node-b",
                )]),
            ),
        ],
    );
    assert_eq!(conflict, vec![secret_path]);
    let outage_ctx = OperationContext::new();
    cache.reconcile_from_in(
        &outage_ctx,
        "/",
        vec![("node-a".into(), ListingOutcome::Unreachable)],
    );
    assert_eq!(
        cache.get_entry_by_path(secret_path).unwrap().availability,
        Availability::Inaccessible
    );
    let recovery_ctx = OperationContext::new();
    cache.reconcile_from_in(
        &recovery_ctx,
        "/",
        vec![("node-a".into(), ListingOutcome::Answered(vec![entry]))],
    );
    assert!(cache.rename_entry_in(&recovery_ctx, secret_path, "/also-private"));
    assert!(
        cache
            .remove_entry_in(&recovery_ctx, "/also-private")
            .is_some()
    );
    assert!(guard.shutdown(Duration::from_secs(2)));
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains(secret_path)
            && !raw.contains("also-private")
            && !raw.contains("private-guid")
    );
    let records = events(&path);
    let has = |message: &str, severity: &str, ctx: &OperationContext| {
        records.iter().any(|event| {
            event["message"] == message
                && event["severity"] == severity
                && event["correlation_id"] == ctx.id().to_string()
                && event["component"] == "metadata_cache"
                && event["console"] == false
        })
    };
    assert!(
        records
            .iter()
            .any(|e| e["operation"] == "initialize" && e["correlation_id"].is_null())
    );
    assert!(has(
        "conflicting names detected across nodes",
        "warning",
        &conflict_ctx
    ));
    assert!(has(
        "backing node did not answer; cached entries retained",
        "warning",
        &outage_ctx
    ));
    assert!(has(
        "no backing node answered the listing",
        "error",
        &outage_ctx
    ));
    assert!(has(
        "cached entries became available again",
        "info",
        &recovery_ctx
    ));
    assert!(has(
        "metadata cache rename completed",
        "debug",
        &recovery_ctx
    ));
    assert!(has(
        "metadata cache removal completed",
        "debug",
        &recovery_ctx
    ));
    assert!(records.iter().any(|e| e["operation"] == "reconcile"
        && e["correlation_id"] == recovery_ctx.id().to_string()
        && e["fields"]["entries_recovered"] == 1));
}

#[cfg(target_os = "linux")]
#[test]
fn cache_outcomes_do_not_depend_on_a_failing_log_sink() {
    let (logger, mut guard) = telemetry::start(logger_config(
        std::path::Path::new("/dev/full"),
        Severity::Info,
    ))
    .unwrap();
    let mut cache = MetadataCache::with_logger(logger.clone());
    let ctx = OperationContext::new();
    cache.upsert_entry_in(&ctx, event_test_entry("/file", "guid", "node-a"));
    assert!(
        cache
            .reconcile_from_in(
                &ctx,
                "/",
                vec![("node-a".into(), ListingOutcome::Unreachable)]
            )
            .is_empty()
    );
    assert_eq!(
        cache.get_entry_by_path("/file").unwrap().availability,
        Availability::Inaccessible
    );
    assert!(logger.flush(Duration::from_secs(2)));
    assert!(logger.stats().sink_failures > 0);
    let _ = guard.shutdown(Duration::from_secs(2));
}

#[test]
fn cache_ttl_and_max_size_evict_memory_but_keep_redb_inodes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.redb");
    let db =
        std::sync::Arc::new(punkfs::core::db::persistent::PersistentDb::open(&db_path).unwrap());
    let mut cache = MetadataCache::new();
    cache
        .attach_db(db, Some(Duration::from_millis(40)), Some(2))
        .unwrap();
    cache.insert_root("/", "/", FsEntryKind::Directory);
    for (path, guid) in [("/one", "g1"), ("/two", "g2"), ("/three", "g3")] {
        cache.upsert_entry(FsEntry {
            inode: 0,
            path: path.into(),
            guid: guid.into(),
            kind: FsEntryKind::File,
            node_id: "node-a".into(),
            availability: Availability::Available,
            attr: FileAttrData {
                guid: guid.into(),
                kind: FsEntryKind::File,
                ..Default::default()
            },
        });
    }
    assert!(cache.entries.len() <= 2);
    let ino_one = cache.lookup_inode("/one").unwrap();

    std::thread::sleep(Duration::from_millis(60));
    cache.prune_expired();
    assert!(
        cache.entries.iter().all(|e| e.path == "/"),
        "expired entries should be evicted from memory"
    );
    assert_eq!(cache.lookup_inode("/one"), Some(ino_one));
    assert_eq!(cache.lookup_path(ino_one).as_deref(), Some("/one"));

    // Rename preserves the constant inode; remove retires it and allocates a new one on recreate.
    assert!(cache.rename_entry("/one", "/one-renamed"));
    assert_eq!(cache.lookup_inode("/one-renamed"), Some(ino_one));
    cache.remove_entry("/one-renamed");
    cache.upsert_entry(FsEntry {
        inode: 0,
        path: "/one-renamed".into(),
        guid: "g1-new".into(),
        kind: FsEntryKind::File,
        node_id: "node-a".into(),
        availability: Availability::Available,
        attr: FileAttrData {
            guid: "g1-new".into(),
            kind: FsEntryKind::File,
            ..Default::default()
        },
    });
    assert_ne!(cache.lookup_inode("/one-renamed"), Some(ino_one));
}

#[test]
fn persisted_listing_reconciles_after_eviction_without_deleting_other_owner() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(
        punkfs::core::db::persistent::PersistentDb::open(&dir.path().join("cache.redb")).unwrap(),
    );
    let mut cache = MetadataCache::new();
    cache
        .attach_db(db, Some(Duration::from_millis(1)), Some(1))
        .unwrap();
    cache.insert_root("/", "/", FsEntryKind::Directory);
    let old = event_test_entry("/old", "stable-guid", "node-a");
    let other = event_test_entry("/other", "other-guid", "node-b");
    cache.upsert_entry(old.clone());
    cache.upsert_entry(other);
    let inode = cache.lookup_inode("/old").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let mut renamed = old;
    renamed.path = "/new".into();
    assert!(
        cache
            .reconcile_from(
                "/",
                vec![("node-a".into(), ListingOutcome::Answered(vec![renamed]))]
            )
            .is_empty()
    );
    assert_eq!(cache.lookup_inode("/new"), Some(inode));
    assert_eq!(cache.lookup_inode("/old"), None);
    assert!(cache.lookup_inode("/other").is_some());
    assert!(cache.entries.len() <= 1);
}

#[test]
fn cache_events_obey_info_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let (logger, mut guard) = telemetry::start(logger_config(&path, Severity::Info)).unwrap();
    let mut cache = MetadataCache::with_logger(logger);
    let ctx = OperationContext::new();
    cache.upsert_entry_in(&ctx, event_test_entry("/file", "guid", "node-a"));
    cache.reconcile_from_in(
        &ctx,
        "/",
        vec![("node-a".into(), ListingOutcome::Unreachable)],
    );
    assert!(guard.shutdown(Duration::from_secs(2)));
    let records = events(&path);
    assert!(records.iter().any(|e| e["severity"] == "warning"));
    assert!(
        records
            .iter()
            .all(|e| e["severity"] != "debug" && e["severity"] != "trace")
    );
}
