use structopt::{clap, StructOpt};

#[derive(StructOpt)]
#[structopt(about = "NET Server")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
pub struct Config {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    pub address: url::Url,
    #[structopt(long, env = "NET_IP_CHECKER_PORT")]
    pub ip_checker_port: u16,
}
