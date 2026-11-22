//! Minimal manual-test mount: `cargo run --example mount -- <server_url> <mountpoint>`
//! Keeps the mount alive until Ctrl-C. Example:
//!   cargo run --example mount -- http://127.0.0.1:10000 /tmp/punkfs-mount

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry =
        punkfs::telemetry::init_global(punkfs::telemetry::Config::from_env("punkfs-mount")?)?;
    let mut args = std::env::args().skip(1);
    let server_url = args.next().expect("usage: mount <server_url> <mountpoint>");
    let mountpoint: PathBuf = args
        .next()
        .expect("usage: mount <server_url> <mountpoint>")
        .into();

    let node = punkfs::FsNode {
        node_id: "manual-test".into(),
    };
    let _session = punkfs::PunkFs::mount(&server_url, &mountpoint, node)?;
    println!(
        "mounted {server_url} at {}; Ctrl-C to unmount",
        mountpoint.display()
    );
    std::thread::sleep(std::time::Duration::MAX);
    Ok(())
}
