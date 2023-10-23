use std::time::Duration;
use clap::{Parser};


#[derive(Parser)]
#[command(version, about = "NET Server", long_about)]
pub struct Config {
    #[arg(short = 'a', env = "NET_ADDRESS")]
    pub address: url::Url,
    #[arg(long, env = "NET_IP_CHECKER_PORT", default_value = "0")]
    pub ip_checker_port: u16,
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "10s")]
    pub session_cleaner_interval: Duration,
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "1min")]
    pub session_timeout: chrono::Duration,
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "10min")]
    pub session_purge_timeout: chrono::Duration,
    #[arg(long, env, default_value = "1073741824")]
    pub forwarder_rate_limit: u32,
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "1s")]
    pub forwarder_resume_interval: Duration,
    #[arg(long, env, default_value = "127.0.0.1:9000")]
    pub metrics_scrape_addr: std::net::SocketAddr,
    #[arg(long, env, value_parser = parse_humantime_to_chrono, default_value = "2s")]
    pub drop_packets_older: chrono::Duration,
    #[arg(long, env, value_parser = parse_humantime_to_chrono, default_value = "500ms")]
    pub drop_forward_packets_older: chrono::Duration,
    #[arg(long, env, default_value = "16")]
    pub workers : usize,
    #[arg(long, env, default_value = "32")]
    pub tasks_per_worker : usize
}

pub fn parse_humantime_to_chrono(s: &str) -> Result<chrono::Duration, anyhow::Error> {
    Ok(chrono::Duration::from_std(humantime::parse_duration(s)?)?)
}



#[test]
fn verify_cli() {
    use clap::CommandFactory;
    Config::command().debug_assert()
}