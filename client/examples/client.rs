use structopt::{clap, StructOpt};

use ya_relay_client::ClientBuilder;
use ya_relay_core::crypto::FallbackCryptoProvider;
use ya_relay_core::key::{load_or_generate, Protected};
use ya_relay_core::NodeId;

#[derive(StructOpt)]
#[structopt(about = "NET Client")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
struct Options {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    pub address: url::Url,
    #[structopt(short = "f", long, env = "CLIENT_KEY_FILE")]
    key_file: Option<String>,
    #[structopt(short = "p", long, env = "CLIENT_KEY_PASSWORD", parse(from_str = Protected::from))]
    key_password: Option<Protected>,
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
pub struct Ping {}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct Init {}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct FindNode {
    #[structopt(short = "n", long, env)]
    node_id: NodeId,
}

async fn run() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    std::env::set_var(
        "RUST_LOG",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "trace,mio=info,smoltcp=info".to_string()),
    );
    env_logger::init();

    let args = Options::from_args();

    let address = args.address.clone();
    let builder = if let Some(key_file) = args.key_file {
        let password = args.key_password.clone();
        let secret = load_or_generate(&key_file, password);
        ClientBuilder::from_url(address).crypto(FallbackCryptoProvider::new(secret))
    } else {
        ClientBuilder::from_url(address)
    };

    let client = builder.build().await?;

    log::info!("Sending to server listening on: {}", args.address);

    match args.commands {
        Commands::Init(Init {}) => {
            let session = client.sessions.server_session().await?;
            let endpoints = session.register_endpoints(vec![]).await?;

            log::info!("Discovered public endpoints: {:?}", endpoints);
        }
        Commands::FindNode(opts) => {
            let session = client.sessions.server_session().await?;
            session.find_node(opts.node_id).await?;
        }
        Commands::Ping(_) => {
            let session = client.sessions.server_session().await?;
            session.ping().await?;
        }
    };

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();
    local_set.run_until(run()).await
}
