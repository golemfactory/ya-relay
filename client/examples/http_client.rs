use std::{convert::TryFrom, str::FromStr};

use actix_web::{get, post, web, App, HttpResponse, HttpServer, Responder};
use anyhow::Result;
use structopt::StructOpt;

use ya_relay_client::{Client, ClientBuilder, FailFast};
use ya_relay_core::{
    crypto::FallbackCryptoProvider,
    key::{load_or_generate, Protected},
    NodeId,
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

#[derive(Clone)]
struct AppState {
    client: std::sync::Arc<std::sync::Mutex<Client>>,
}

#[get("/find-node/{node_id}")]
async fn find_node(node_id: web::Path<String>) -> impl Responder {
    let node_id = node_id.parse::<NodeId>().unwrap();

    let msg = format!("Node id: {node_id} found in s\n");

    HttpResponse::Ok().body(msg)
}
#[tokio::main(flavor = "current_thread")]
// #[actix_web::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::from_args();

    let client = build_client(cli.relay_addr, &cli.key_file, cli.password).await?;

    let app_data = std::sync::Arc::new(std::sync::Mutex::new(client));

    let server = HttpServer::new(|| App::new().app_data(app_data).service(find_node));

    server.bind(("0.0.0.0", cli.port))?.run().await?;

    Ok(())
}

async fn build_client(
    relay_addr: url::Url,
    key_file: &str,
    password: Option<Protected>,
) -> Result<Client> {
    let secret = load_or_generate(&key_file, password);
    let builder = ClientBuilder::from_url(relay_addr).crypto(FallbackCryptoProvider::new(secret));

    let client = builder.connect(FailFast::Yes).build().await?;

    log::info!("CLIENT NODE ID: {}", client.node_id());
    log::info!("CLIENT BIND ADDR: {:?}", client.bind_addr().await);
    log::info!("CLIENT PUBLIC ADDR: {:?}", client.public_addr().await);

    Ok(client)
}
