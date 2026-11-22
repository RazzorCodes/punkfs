//! Mount several backing nodes as one merged namespace.
//!
//! Usage:
//!   cargo run --example mount-cluster -- <mountpoint> <node_id>=<url> [<node_id>=<url> ...]
//!
//! Example, with two servers already running:
//!   punkfs-server /srv/backing-a 127.0.0.1:10001 node-a
//!   punkfs-server /srv/backing-b 127.0.0.1:10002 node-b
//!   cargo run --example mount-cluster -- /tmp/punkfs-mnt \
//!       node-a=http://127.0.0.1:10001 node-b=http://127.0.0.1:10002
//!
//! Startup order matters not at all: connections are lazy, so a node that
//! is down simply contributes nothing until it answers.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mountpoint: PathBuf = args
        .next()
        .expect("usage: mount-cluster <mountpoint> <node_id>=<url> ...")
        .into();

    let nodes: Vec<(String, String)> = args
        .map(|arg| {
            let (id, url) = arg
                .split_once('=')
                .unwrap_or_else(|| panic!("expected <node_id>=<url>, got {arg:?}"));

            // Scheme is optional on the command line, required by the
            // endpoint builder.
            let url = if url.starts_with("http://") || url.starts_with("https://") {
                url.to_owned()
            } else {
                format!("http://{url}")
            };
            (id.to_owned(), url)
        })
        .collect();

    if nodes.is_empty() {
        return Err("need at least one <node_id>=<url>".into());
    }

    for (id, url) in &nodes {
        println!("  node {id} -> {url}");
    }

    let node = punkfs::FsNode {
        node_id: "manual-test".into(),
    };
    let _session = punkfs::PunkFs::mount_cluster(&nodes, &mountpoint, node)?;

    println!(
        "mounted {} node(s) at {}; Ctrl-C to unmount",
        nodes.len(),
        mountpoint.display()
    );
    std::thread::sleep(std::time::Duration::MAX);
    Ok(())
}
