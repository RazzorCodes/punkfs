//! Exercise the public cache and attributes shared across entry kinds.
use std::time::{Duration, UNIX_EPOCH};

use punkfs::store::db::attributes::misc::{Access, FileAttrData, FileTimes, FsEntryKind, Stat};
use punkfs::store::db::metadata_cache::{Availability, FsEntry, ListingOutcome, MetadataCache};
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
