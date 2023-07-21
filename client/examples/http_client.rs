use ya_relay_client::{Client, ClientBuilder, FailFast, ForwardReceiver, GenericSender};
use ya_relay_core::{
    crypto::FallbackCryptoProvider,
    key::{load_or_generate, Protected},
    NodeId,
};

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use actix_web::{
    get,
    web::{self, Data},
    App, HttpResponse, HttpServer, Responder,
};
use anyhow::Result;
use structopt::StructOpt;
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
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

type Pings = Arc<Mutex<HashMap<u32, (Instant, oneshot::Sender<Duration>)>>>;

#[derive(Debug)]
enum Command {
    FindNode(FindNode),
    Ping(Ping),
}

#[derive(Debug)]
struct FindNode {
    node_id: NodeId,
    response: oneshot::Sender<Duration>,
}

#[derive(Debug)]
struct Ping {
    node_id: NodeId,
    response: oneshot::Sender<Duration>,
}

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

#[get("/ping/{node_id}")]
async fn ping(
    node_id: web::Path<String>,
    client_sender: web::Data<Sender<Command>>,
) -> impl Responder {
    let node_id = node_id.parse::<NodeId>().unwrap();

    let (rx, tx) = oneshot::channel::<Duration>();

    client_sender
        .send(Command::Ping(Ping {
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

async fn receiver_task(mut receiver: ForwardReceiver, pings: Pings) -> Result<()> {
    while let Some(fwd) = receiver.recv().await {
        log::info!("Handler got: {fwd:?}");

        match fwd.transport {
            ya_relay_client::TransportType::Reliable => {
                let msg = String::from_utf8(fwd.payload.into_vec()).unwrap();
                match msg.as_str() {
                    "Ping" => {
                        todo!()
                    }
                    "Pong" => {
                        if let Some((ts, sender)) = pings.lock().unwrap().remove(&1) {
                            sender.send(ts.elapsed()).unwrap();
                        }
                    }
                    _ => {}
                }
            }
            ya_relay_client::TransportType::Unreliable => (),
            ya_relay_client::TransportType::Transfer => (),
        }
    }

    Ok(())
}

async fn client_task(client: Client, mut rx: mpsc::Receiver<Command>, pings: Pings) -> Result<()> {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::FindNode(FindNode { node_id, response }) => {
                let now = Instant::now();

                match client.find_node(node_id).await {
                    Ok(node) => {
                        let elapsed = now.elapsed();
                        log::info!("Found node {node:?} in {} ms", elapsed.as_millis());
                        response.send(elapsed).unwrap();
                    }
                    Err(e) => log::error!("Find node failed: {e}"),
                }
            }
            Command::Ping(Ping { node_id, response }) => {
                //TODO choose which transport type in API
                match client.forward_reliable(node_id).await {
                    Ok(mut sender) => {
                        let msg = "Ping";
                        if let Err(e) = sender.send(msg.as_bytes().to_vec().into()).await {
                            log::error!("Send failed: {e}");
                        } else {
                            pings.lock().unwrap().insert(1, (Instant::now(), response));
                        }
                    }
                    Err(e) => log::error!("Forward reliable failed: {e}"),
                }

                //Send ping to other node by forward sender and put response into hashmap

                //forward receiver will
                // If it got ping: response with pingresponse
                // If got pingresponse then search in hashmap and send back duration
            }
        }
    }
    Ok(())
}

async fn run() -> Result<()> {
    env_logger::init();

    let cli = Cli::from_args();

    let (tx, rx) = mpsc::channel::<Command>(16);
    let client = build_client(cli.relay_addr, &cli.key_file, cli.password).await?;

    let receiver = client.forward_receiver().await.unwrap();

    let pings = Pings::default();
    let pings_cloned = pings.clone();

    tokio::task::spawn_local(async move {
        receiver_task(receiver, pings_cloned).await.unwrap();
    });

    tokio::task::spawn_local(async move {
        client_task(client, rx, pings).await.unwrap();
    });

    HttpServer::new(move || {
        App::new()
            .app_data(Data::new(tx.clone()))
            .service(find_node)
            .service(ping)
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
