use crate::server::{ServerConfig, SessionHandlerConfig};
use crate::SessionManagerConfig;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version = env!("YA_RELAY_SERVER_VERSION"),
    about = "NET Server",
    long_about
)]
pub struct Config {
    #[arg(long, env, default_value = "127.0.0.1:9000")]
    pub metrics_scrape_addr: std::net::SocketAddr,
    #[arg(long, env = "STATE_DIRECTORY")]
    pub state_dir: Option<PathBuf>,

    #[command(flatten)]
    pub server: ServerConfig,

    #[command(flatten)]
    pub session_manager: SessionManagerConfig,

    #[command(flatten)]
    pub session_handler: SessionHandlerConfig,

    #[command(flatten)]
    pub ip_check: crate::server::IpCheckerConfig,
}

#[test]
fn verify_cli() {
    use clap::CommandFactory;
    Config::command().debug_assert()
}

#[test]
fn version_contains_build_metadata() {
    use clap::error::ErrorKind;

    let error = match Config::try_parse_from(["ya-relay-server", "--version"]) {
        Ok(_) => panic!("--version unexpectedly parsed as a server configuration"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), ErrorKind::DisplayVersion);
    assert_eq!(
        error.to_string(),
        format!("ya-relay-server {}\n", env!("YA_RELAY_SERVER_VERSION"))
    );
}
