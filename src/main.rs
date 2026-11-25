//! Mount a local directory and merge optional remote punkfs nodes into it.
use punkfs::{FsNode, PunkFs};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry =
        punkfs::telemetry::init_global(punkfs::telemetry::Config::from_env("punkfs")?)?;
    let config = punkfs::config::parser::parse()?;
    if let Some(listen) = config.listen {
        return punkfs::run(punkfs::NodeConfig {
            mountpoint: config.mountpoint,
            node_id: config.node_id,
            listen,
            peers: config.peers,
            route_console: config.route_console,
            db_path: Some(config.db_path),
            cache_ttl: Some(config.cache_ttl),
            cache_max_entries: Some(config.cache_max_entries),
        });
    }
    let node = FsNode {
        node_id: config.node_id,
    };
    let store = std::sync::Arc::new(punkfs::Store::new_local(
        &config.peers,
        node,
        &config.mountpoint,
    )?);
    let _session = PunkFs::mount(
        &config.mountpoint,
        punkfs::MountOptions {
            store,
            route_console: config.route_console,
        },
    )?;
    println!("mounted {}", config.mountpoint.display());
    loop {
        std::thread::park();
    }
}
