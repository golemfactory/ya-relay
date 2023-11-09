use crate::server::{ServerConfig, SessionHandlerConfig};
use crate::SessionManagerConfig;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "NET Server", long_about)]
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
