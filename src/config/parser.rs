//! Command-line parsing and configuration initialization.
use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use super::Config;

#[derive(Default)]
pub(super) struct CliOptions {
    pub config_path: Option<PathBuf>,
    pub positional: Vec<String>,
    pub listen: Option<SocketAddr>,
    pub db_path: Option<PathBuf>,
    pub peers: Vec<(String, String)>,
}

/// Parse the process arguments and initialize the effective configuration.
pub fn parse() -> Result<Config, Box<dyn Error>> {
    let default_file = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("punkfs.toml")));
    let routes = match std::env::var("PUNKFS_LOG_ROUTES") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => Some(String::new()),
    };
    let result = parse_from(
        std::env::args().skip(1),
        default_file.as_deref(),
        routes.as_deref(),
    );
    if let Some(logger) = crate::telemetry::global() {
        let (severity, message) = if result.is_ok() {
            (
                crate::telemetry::Severity::Info,
                "configuration initialized",
            )
        } else {
            (
                crate::telemetry::Severity::Error,
                "configuration initialization failed",
            )
        };
        logger.emit(crate::telemetry::Event::new(severity, "config", message));
    }
    result
}

/// Parse arguments excluding the program name, with explicit environment inputs.
/// This allows embedding and deterministic tests without mutating process state.
pub fn parse_from<I, S>(
    args: I,
    default_file: Option<&Path>,
    routes: Option<&str>,
) -> Result<Config, Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut cli = CliOptions::default();
    let mut args = args.into_iter().map(Into::into);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                if cli.config_path.is_some() {
                    return Err("--config may only be supplied once".into());
                }
                cli.config_path = Some(PathBuf::from(
                    args.next().ok_or("--config requires a path")?,
                ));
            }
            "--listen" => {
                if cli.listen.is_some() {
                    return Err("--listen may only be supplied once".into());
                }
                cli.listen = Some(args.next().ok_or("--listen requires an address")?.parse()?);
            }
            "--db-path" => {
                if cli.db_path.is_some() {
                    return Err("--db-path may only be supplied once".into());
                }
                cli.db_path = Some(PathBuf::from(
                    args.next().ok_or("--db-path requires a path")?,
                ));
            }
            _ => {
                if let Some((id, url)) = arg.split_once('=') {
                    if id.is_empty() || url.is_empty() {
                        return Err("remote node id and URL must not be empty".into());
                    }
                    cli.peers.push((id.to_owned(), url.to_owned()));
                } else {
                    cli.positional.push(arg);
                }
            }
        }
    }
    super::initialize(cli, default_file, routes)
}
