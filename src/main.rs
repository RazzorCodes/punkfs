//! Mount a local directory and merge optional remote punkfs nodes into it.
use punkfs::core::config::NodeTomlConfig;
use punkfs::{FsNode, PunkFs};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry =
        punkfs::telemetry::init_global(punkfs::telemetry::Config::from_env("punkfs")?)?;
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    let mut config_path: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut cli_listen: Option<std::net::SocketAddr> = None;
    let mut cli_db_path: Option<PathBuf> = None;
    let mut cli_peers: Vec<(String, String)> = Vec::new();

    let mut i = 0;
    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "--config" => {
                if config_path.is_some() {
                    return Err("--config may only be supplied once".into());
                }
                i += 1;
                config_path = Some(PathBuf::from(
                    raw_args.get(i).ok_or("--config requires a path")?,
                ));
            }
            "--listen" => {
                if cli_listen.is_some() {
                    return Err("--listen may only be supplied once".into());
                }
                i += 1;
                cli_listen = Some(
                    raw_args
                        .get(i)
                        .ok_or("--listen requires an address")?
                        .parse()?,
                );
            }
            "--db-path" => {
                if cli_db_path.is_some() {
                    return Err("--db-path may only be supplied once".into());
                }
                i += 1;
                cli_db_path = Some(PathBuf::from(
                    raw_args.get(i).ok_or("--db-path requires a path")?,
                ));
            }
            arg => {
                if let Some((id, url)) = arg.split_once('=') {
                    if id.is_empty() || url.is_empty() {
                        return Err("remote node id and URL must not be empty".into());
                    }
                    cli_peers.push((id.to_owned(), url.to_owned()));
                } else {
                    positional.push(arg.to_owned());
                }
            }
        }
        i += 1;
    }

    let default_toml = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("punkfs.toml")));
    let file_cfg = if let Some(path) = config_path {
        NodeTomlConfig::from_file(&path)?
    } else if let Some(path) = default_toml.filter(|p| p.exists()) {
        NodeTomlConfig::from_file(&path)?
    } else {
        NodeTomlConfig::default()
    };

    let mountpoint = positional
        .first()
        .map(PathBuf::from)
        .or(file_cfg.mountpoint)
        .ok_or("usage: punkfs [--config <file.toml>] <mountpoint> <local-node-id> [--listen <address>] [--db-path <file.redb>] [<remote-node-id>=<url> ...]")?;
    let local = positional
        .get(1)
        .cloned()
        .or(file_cfg.node_id)
        .ok_or("missing local node id")?;
    if local.is_empty() {
        return Err("local node id must not be empty".into());
    }
    let listen = cli_listen.or(file_cfg.listen);
    let nodes = if cli_peers.is_empty() {
        file_cfg.peers
    } else {
        cli_peers
    };
    let db_path = cli_db_path
        .or(file_cfg.db_path)
        .unwrap_or_else(|| NodeTomlConfig::default_db_path(&local));

    let route_console = match std::env::var("PUNKFS_LOG_ROUTES").as_deref() {
        Ok("1") | Ok("true") => true,
        Ok("0") | Ok("false") => false,
        Err(std::env::VarError::NotPresent) => file_cfg.route_console.unwrap_or(false),
        _ => return Err("PUNKFS_LOG_ROUTES must be 1, 0, true, or false".into()),
    };
    if let Some(listen) = listen {
        return punkfs::core::node::run(punkfs::core::node::NodeConfig {
            mountpoint,
            node_id: local,
            listen,
            peers: nodes,
            route_console,
            db_path: Some(db_path),
            cache_ttl: Some(file_cfg.cache_ttl),
            cache_max_entries: Some(file_cfg.cache_max_entries),
        });
    }
    let node = FsNode { node_id: local };
    let _session = if route_console {
        PunkFs::mount_with_local_console(&nodes, &mountpoint, node)?
    } else {
        PunkFs::mount_with_local(&nodes, &mountpoint, node)?
    };
    println!("mounted {}", mountpoint.display());
    loop {
        std::thread::park();
    }
}
