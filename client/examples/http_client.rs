use std::{
    convert::TryFrom,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use actix_web::{
    get, post,
    web::{self, Data},
    App, HttpResponse, HttpServer, Responder,
};
use anyhow::Result;
use futures::TryFutureExt;
use structopt::StructOpt;

use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
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

#[derive(Debug)]
enum Command {
    FindNode(FindNode),
}

#[derive(Debug)]
struct FindNode {
    node_id: NodeId,
    response: oneshot::Sender<Duration>,
}

#[derive(Clone)]
struct AppState {}

#[get("/find-node/{node_id}")]
async fn find_node(
    node_id: web::Path<String>,
    client_sender: web::Data<Sender<Command>>,
) -> impl Responder {
    let node_id = node_id.parse::<NodeId>().unwrap();

    let (rx, tx) = oneshot::channel::<Duration>();

    client_sender
        .send(Command::FindNode(FindNode {
            node_id,
            response: rx,
        }))
        .await
        .unwrap();

    let duration = tx.await.unwrap();

    HttpResponse::Ok().body(format!(
        "Node id: {node_id} found in {} ms\n",
        duration.as_millis()
    ))
}

async fn run() -> Result<()> {
    env_logger::init();

    let cli = Cli::from_args();

    let (rx, mut tx) = mpsc::channel::<Command>(16);
    let client = build_client(cli.relay_addr, &cli.key_file, cli.password).await?;

    let p2p_client = tokio::task::spawn_local(async move {
        client.node_id();
        while let Some(cmd) = tx.recv().await {
            // log::info!("client got {cmd:?}");

            match cmd {
                Command::FindNode(FindNode { node_id, response }) => {
                    let now = Instant::now();

                    match client.find_node(node_id).await {
                        Ok(node) => {
                            let elapsed = now.elapsed();
                            log::info!("Found node {node:?} in {} ms", elapsed.as_millis());
                            response.send(elapsed).unwrap();
                        }
                        Err(e) => log::error!("{e}"),
                    }
                }
            }
        }
    });

    let http_server = HttpServer::new(move || {
        App::new()
            .app_data(Data::new(rx.clone()))
            .service(find_node)
    })
    .workers(4)
    .bind(("0.0.0.0", cli.port))?
    .run()
    .await?;

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

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();
    local_set.run_until(run()).await
}
