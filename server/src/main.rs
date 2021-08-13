pub(crate) mod challenge;
pub(crate) mod error;
pub(crate) mod packets;
mod server;
pub(crate) mod session;

use server::Server;

use structopt::{clap, StructOpt};

#[derive(StructOpt)]
#[structopt(about = "NET Server")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
struct Options {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    address: url::Url,
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Options::from_args();

    let server = Server::bind(args.address).await?;
    server.run().await
}
