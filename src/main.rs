//! Mount a local directory and merge optional remote punkfs nodes into it.
use punkfs::{FsNode, PunkFs};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry =
        punkfs::telemetry::init_global(punkfs::telemetry::Config::from_env("punkfs")?)?;
    let mut args = std::env::args().skip(1);
    let mountpoint = PathBuf::from(
        args.next()
            .ok_or("usage: punkfs <mountpoint> <local-node-id> [<remote-node-id>=<url> ...]")?,
    );
    let local = args.next().ok_or("missing local node id")?;
    let mut nodes = Vec::new();
    for arg in args {
        let (id, url) = arg
            .split_once('=')
            .ok_or("expected <remote-node-id>=<url>")?;
        if id.is_empty() || url.is_empty() {
            return Err("remote node id and URL must not be empty".into());
        }
        nodes.push((id.to_owned(), url.to_owned()));
    }
    if local.is_empty() {
        return Err("local node id must not be empty".into());
    }
    let route_console = match std::env::var("PUNKFS_LOG_ROUTES").as_deref() {
        Ok("1") | Ok("true") => true,
        Ok("0") | Ok("false") | Err(std::env::VarError::NotPresent) => false,
        _ => return Err("PUNKFS_LOG_ROUTES must be 1, 0, true, or false".into()),
    };
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
