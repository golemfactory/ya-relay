use std::time::Duration;
use structopt::{clap, StructOpt};

#[derive(StructOpt)]
#[structopt(about = "NET Server")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
pub struct Config {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    pub address: url::Url,
    #[structopt(long, env = "NET_IP_CHECKER_PORT")]
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
}

pub fn parse_humantime_to_chrono(s: &str) -> Result<chrono::Duration, anyhow::Error> {
    Ok(chrono::Duration::from_std(humantime::parse_duration(s)?)?)
}
