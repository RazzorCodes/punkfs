use punkfs::config::parser::parse_from;
use std::time::Duration;

#[test]
fn parser_initializes_cli_overrides_and_relative_toml_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("punkfs.toml");
    std::fs::write(
        &path,
        r#"
mountpoint = "backing"
node_id = "toml-node"
listen = "127.0.0.1:6000"
db_path = "state/catalog.redb"
cache_ttl_ms = 12
cache_max_entries = 9
route_console = true
[peers]
old = "http://127.0.0.1:1"
"#,
    )
    .unwrap();
    let config = parse_from(
        [
            "--config",
            path.to_str().unwrap(),
            "/cli",
            "cli-node",
            "--listen",
            "127.0.0.1:7000",
            "--db-path",
            "cli.redb",
            "new=http://127.0.0.1:2",
        ],
        None,
        Some("false"),
    )
    .unwrap();
    assert_eq!(config.mountpoint, std::path::Path::new("/cli"));
    assert_eq!(config.node_id, "cli-node");
    assert_eq!(config.listen.unwrap().port(), 7000);
    assert_eq!(config.db_path, std::path::Path::new("cli.redb"));
    assert_eq!(
        config.peers,
        vec![("new".into(), "http://127.0.0.1:2".into())]
    );
    assert_eq!(config.cache_ttl, Duration::from_millis(12));
    assert_eq!(config.cache_max_entries, 9);
    assert!(!config.route_console);

    let file = parse_from(std::iter::empty::<String>(), Some(&path), None).unwrap();
    assert_eq!(file.mountpoint, dir.path().join("backing"));
    assert_eq!(file.db_path, dir.path().join("state/catalog.redb"));
    assert_eq!(file.node_id, "toml-node");
    assert!(file.route_console);
}

#[test]
fn parser_rejects_invalid_input_and_missing_explicit_file() {
    assert!(parse_from(["--listen"], None, None).is_err());
    assert!(
        parse_from(
            ["--listen", "127.0.0.1:1", "--listen", "127.0.0.1:2"],
            None,
            None
        )
        .is_err()
    );
    assert!(parse_from(["/mnt", "node"], None, Some("yes")).is_err());
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.toml");
    assert!(
        parse_from(
            ["--config", missing.to_str().unwrap(), "/mnt", "node"],
            None,
            None
        )
        .is_err()
    );
}

#[test]
fn database_initializer_opens_persists_and_reports_open_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.redb");
    let db = punkfs::db::open(&path).unwrap();
    assert!(path.exists());
    drop(db);
    assert!(punkfs::db::open(&path).is_ok());
    assert!(punkfs::db::open(dir.path()).is_err());
}
