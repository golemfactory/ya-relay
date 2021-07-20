use env_logger;
use std::convert::TryFrom;
use structopt::{clap, StructOpt};

use ya_client_model::NodeId;
use ya_net_server::testing::Client;
use ya_net_server::SessionId;

#[derive(StructOpt)]
#[structopt(about = "NET Client")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
struct Options {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    pub address: url::Url,
    #[structopt(subcommand)]
    pub commands: Commands,
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub enum Commands {
    Init(Init),
    FindNode(FindNode),
    Ping(Ping),
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct Ping {
    #[structopt(short = "s", long, env)]
    session_id: String,
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct Init {
    #[structopt(short = "n", long, env)]
    pub node_id: NodeId,
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct FindNode {
    #[structopt(short = "n", long, env)]
    node_id: NodeId,
    #[structopt(short = "s", long, env)]
    session_id: String,
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Options::from_args();

    let client = Client::bind(args.address.clone()).await?;

    log::info!("Sending to server listening on: {}", args.address);

    match args.commands {
        Commands::Init(Init { node_id }) => {
            client.init_session(node_id).await?;
        }
        Commands::FindNode(opts) => {
            client
                .find_node(SessionId::try_from(opts.session_id.as_str())?, opts.node_id)
                .await?;
        }
        Commands::Ping(opts) => {
            client
                .ping(SessionId::try_from(opts.session_id.as_str())?)
                .await?;
        }
    };

    Ok(())
}
