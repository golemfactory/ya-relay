use anyhow::Result;
use structopt::StructOpt;

use ya_relay_client::{Client, ClientBuilder, FailFast};
use ya_relay_core::{
    crypto::FallbackCryptoProvider,
    key::{load_or_generate, Protected},
};

#[derive(StructOpt)]
struct Cli {
    #[structopt(long, env = "PORT")]
    port: u16,
    #[structopt(long, env = "RELAY_ADDR")]
    relay_addr: url::Url,
    #[structopt(long, env = "KEY_FILE")]
    key_file: String,
    #[structopt(long, env = "PASSWORD", parse(from_str = Protected::from))]
    password: Option<Protected>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::from_args();

    let client = build_client(cli.relay_addr, &cli.key_file, cli.password).await?;

    Ok(())
}

async fn build_client(
    relay_addr: url::Url,
    key_file: &str,
    password: Option<Protected>,
) -> Result<Client> {
    let secret = load_or_generate(&key_file, password);
    let builder = ClientBuilder::from_url(relay_addr).crypto(FallbackCryptoProvider::new(secret));

    builder.connect(FailFast::Yes).build().await
}
