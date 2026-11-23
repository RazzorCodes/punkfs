//! punkfs-server: serve one local folder over grpc.
//!
//! Usage: `punkfs-server [root_dir] [listen_addr] [node_id]`
//!
//! The heavy lifting lives in [`punkfs::grpc::service`] so it can be
//! constructed directly by tests; this binary only parses arguments,
//! binds, and serves.
use std::path::PathBuf;

use tonic::transport::Server;

use punkfs::grpc::service::PunkFsService;
use punkfs::punk_fs::punk_fs_server::PunkFsServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry =
        punkfs::telemetry::init_global(punkfs::telemetry::Config::from_env("punkfs-server")?)?;
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let addr = args
        .next()
        .unwrap_or_else(|| "[::1]:10000".into())
        .parse()?;
    let node_id = args.next().unwrap_or_else(|| "punkfs-server".into());

    let root = tokio::fs::canonicalize(&root).await?;
    if !tokio::fs::metadata(&root).await?.is_dir() {
        return Err(format!("{} is not a directory", root.display()).into());
    }
    println!("serving {} on {addr} as {node_id}", root.display());

    let service = PunkFsService::try_new(root, node_id)?;

    // Select on the shutdown rpc so a test (or an operator with the
    // dev-shutdown feature) can stop the server cleanly. Without the
    // feature the rpc answers UNIMPLEMENTED and this arm never fires.
    let shutdown = service.clone();
    tokio::select! {
        result = Server::builder()
            .add_service(PunkFsServer::from_arc(service))
            .serve(addr) => result?,
        _ = shutdown.shutdown_signal() => {
            println!("shutdown requested; exiting");
        }
    }

    Ok(())
}
