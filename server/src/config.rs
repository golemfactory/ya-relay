use std::time::Duration;
use structopt::{clap, StructOpt};

#[derive(StructOpt)]
#[structopt(about = "NET Server")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
pub struct Config {
    #[structopt(
        short = "a",
        env = "NET_ADDRESS",
        default_value = "udp://127.0.0.1:7464"
    )]
    pub address: url::Url,
    #[structopt(long, env = "NET_IP_CHECKER_PORT", default_value = "7465")]
    pub ip_checker_port: u16,
    #[structopt(long, env, parse(try_from_str = humantime::parse_duration), default_value = "10s")]
    pub session_cleaner_interval: Duration,
    #[structopt(long, env, parse(try_from_str = parse_humantime_to_chrono), default_value = "1min")]
    pub session_timeout: chrono::Duration,
    #[structopt(long, env, parse(try_from_str = parse_humantime_to_chrono), default_value = "10min")]
    pub session_purge_timeout: chrono::Duration,
    #[structopt(long, env, default_value = "1073741824")]
    pub forwarder_rate_limit: u32,
    #[structopt(long, env, parse(try_from_str = humantime::parse_duration), default_value = "1s")]
    pub forwarder_resume_interval: Duration,
    #[structopt(long, env, default_value = "127.0.0.1:9000")]
    pub metrics_scrape_addr: std::net::SocketAddr,
    #[structopt(long, env, parse(try_from_str = parse_humantime_to_chrono), default_value = "2s")]
    pub drop_packets_older: chrono::Duration,
    #[structopt(long, env, parse(try_from_str = parse_humantime_to_chrono), default_value = "500ms")]
    pub drop_forward_packets_older: chrono::Duration,
}

pub fn parse_humantime_to_chrono(s: &str) -> Result<chrono::Duration, anyhow::Error> {
    Ok(chrono::Duration::from_std(humantime::parse_duration(s)?)?)
}
