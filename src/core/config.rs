//! Per-process TOML configuration for a punkfs node.
//!
//! Loaded from `<exe_dir>/punkfs.toml` when present or `--config <path>`,
//! with CLI flags overriding individual fields.

use std::collections::HashSet;
use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use toml_edit::{DocumentMut, Item};

pub const DEFAULT_CACHE_TTL_MS: u64 = 30_000;
pub const DEFAULT_CACHE_MAX_ENTRIES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeTomlConfig {
    pub mountpoint: Option<PathBuf>,
    pub node_id: Option<String>,
    pub listen: Option<SocketAddr>,
    pub db_path: Option<PathBuf>,
    pub peers: Vec<(String, String)>,
    pub cache_ttl: Duration,
    pub cache_max_entries: usize,
    pub route_console: Option<bool>,
}

impl Default for NodeTomlConfig {
    fn default() -> Self {
        Self {
            mountpoint: None,
            node_id: None,
            listen: None,
            db_path: None,
            peers: Vec::new(),
            cache_ttl: Duration::from_millis(DEFAULT_CACHE_TTL_MS),
            cache_max_entries: DEFAULT_CACHE_MAX_ENTRIES,
            route_console: None,
        }
    }
}

impl NodeTomlConfig {
    pub fn from_file(path: &Path) -> Result<Self, Box<dyn Error>> {
        let content = std::fs::read_to_string(path)?;
        Self::parse_toml(&content, Some(path))
    }

    pub fn parse_toml(content: &str, source: Option<&Path>) -> Result<Self, Box<dyn Error>> {
        let doc: DocumentMut = content.parse()?;
        let mut cfg = Self::default();

        if let Some(mountpoint) = get_string(&doc, "mountpoint")? {
            cfg.mountpoint = Some(resolve_relative(source, Path::new(&mountpoint)));
        }
        if let Some(node_id) = get_string(&doc, "node_id")? {
            if node_id.is_empty() {
                return Err("config node_id must not be empty".into());
            }
            cfg.node_id = Some(node_id);
        }
        if let Some(listen) = get_string(&doc, "listen")? {
            cfg.listen = Some(listen.parse()?);
        }
        if let Some(db_path) = get_string(&doc, "db_path")? {
            cfg.db_path = Some(resolve_relative(source, Path::new(&db_path)));
        }
        if let Some(ttl_ms) = get_int(&doc, &["cache_ttl_ms"])? {
            if ttl_ms < 0 {
                return Err("cache_ttl_ms must be non-negative".into());
            }
            cfg.cache_ttl = Duration::from_millis(ttl_ms as u64);
        } else if let Some(ttl_ms) = get_int(&doc, &["cache", "ttl_ms"])? {
            if ttl_ms < 0 {
                return Err("cache.ttl_ms must be non-negative".into());
            }
            cfg.cache_ttl = Duration::from_millis(ttl_ms as u64);
        }
        if let Some(max_entries) = get_int(&doc, &["cache_max_entries"])? {
            if max_entries <= 0 {
                return Err("cache_max_entries must be positive".into());
            }
            cfg.cache_max_entries = max_entries as usize;
        } else if let Some(max_entries) = get_int(&doc, &["cache", "max_entries"])? {
            if max_entries <= 0 {
                return Err("cache.max_entries must be positive".into());
            }
            cfg.cache_max_entries = max_entries as usize;
        }
        if let Some(item) = doc.get("route_console") {
            let value = item
                .as_bool()
                .ok_or("config route_console must be a boolean")?;
            cfg.route_console = Some(value);
        }

        if let Some(peers_item) = doc.get("peers") {
            let mut seen = HashSet::new();
            if let Some(table) = peers_item.as_table_like() {
                for (id, value) in table.iter() {
                    let url = value
                        .as_str()
                        .ok_or("peers table values must be URL strings")?;
                    validate_peer(id, url, &mut seen)?;
                    cfg.peers.push((id.to_owned(), url.to_owned()));
                }
            } else if let Some(array) = peers_item.as_array_of_tables() {
                for entry in array {
                    let id = entry
                        .get("node_id")
                        .or_else(|| entry.get("id"))
                        .and_then(Item::as_str)
                        .ok_or("[[peers]] entries require node_id")?;
                    let url = entry
                        .get("url")
                        .or_else(|| entry.get("address"))
                        .and_then(Item::as_str)
                        .ok_or("[[peers]] entries require url")?;
                    validate_peer(id, url, &mut seen)?;
                    cfg.peers.push((id.to_owned(), url.to_owned()));
                }
            } else {
                return Err("config peers must be a table or array of tables".into());
            }
        }

        Ok(cfg)
    }

    pub fn default_db_path(node_id: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join(format!("{node_id}.redb"))
    }
}

fn validate_peer(id: &str, url: &str, seen: &mut HashSet<String>) -> Result<(), Box<dyn Error>> {
    if id.is_empty() || url.is_empty() {
        return Err("peer id and url must not be empty".into());
    }
    if !seen.insert(id.to_owned()) {
        return Err(format!("duplicate peer id in config: {id}").into());
    }
    Ok(())
}

fn resolve_relative(source: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_relative()
        && let Some(parent) = source.and_then(Path::parent)
        && !parent.as_os_str().is_empty()
    {
        return parent.join(path);
    }
    path.to_path_buf()
}

fn get_string(doc: &DocumentMut, key: &str) -> Result<Option<String>, Box<dyn Error>> {
    let Some(item) = doc.get(key) else {
        return Ok(None);
    };
    let value = item
        .as_str()
        .ok_or_else(|| format!("config {key} must be a string"))?;
    Ok(Some(value.to_owned()))
}

fn get_int(doc: &DocumentMut, path: &[&str]) -> Result<Option<i64>, Box<dyn Error>> {
    let mut current = doc.as_item();
    for segment in path {
        let Some(next) = current.get(segment) else {
            return Ok(None);
        };
        current = next;
    }
    let value = current
        .as_integer()
        .ok_or_else(|| format!("config {} must be an integer", path.join(".")))?;
    Ok(Some(value))
}
