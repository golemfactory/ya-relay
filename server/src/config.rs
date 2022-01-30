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
    #[structopt(long, env, default_value = "60")]
    pub session_timeout: i64,
}
