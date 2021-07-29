use env_logger;
use std::convert::TryFrom;
use structopt::{clap, StructOpt};

use ya_client_model::NodeId;
use ya_net_server::testing::Client;
use ya_net_server::testing::key::{load_or_generate, Protected};
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
pub struct Init {}

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

    let key_file_name = std::env::var("CLIENT_KEY_FILE").unwrap_or("./client.key.json".to_string());
    let key_file_password = std::env::var("CLIENT_KEY_PASSWORD").ok().map(|x| Protected::from(x));
    let secret = load_or_generate(&key_file_name, key_file_password);
    let client = Client::bind(args.address.clone(), secret).await?;

    log::info!("Sending to server listening on: {}", args.address);

    match args.commands {
        Commands::Init(Init {}) => {
            client.init_session().await?;
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
