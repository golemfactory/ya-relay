use ya_relay_client::{Client, ClientBuilder, FailFast, GenericSender};
use ya_relay_core::{
    crypto::FallbackCryptoProvider,
    key::{load_or_generate, Protected},
    NodeId,
};

use actix_web::{
    get,
    web::{self, Data},
    App, HttpResponse, HttpServer, Responder,
};
use anyhow::{anyhow, Result};
use rand::Rng;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
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

type Pings = Arc<Mutex<HashMap<u32, (Instant, oneshot::Sender<String>)>>>;

#[derive(Debug)]
enum Command {
    FindNode(FindNode),
    Ping(Ping),
}

#[derive(Debug)]
struct FindNode {
    node_id: NodeId,
    response: oneshot::Sender<String>,
}

#[derive(Debug)]
struct Ping {
    node_id: NodeId,
    request_id: u32,
    response: oneshot::Sender<String>,
}

#[get("/find-node/{node_id}")]
async fn find_node(
    node_id: web::Path<String>,
    client_sender: web::Data<Sender<Command>>,
) -> impl Responder {
    let node_id = node_id.parse::<NodeId>().unwrap();

    let (tx, rx) = oneshot::channel::<String>();

    client_sender
        .send(Command::FindNode(FindNode {
            node_id,
            response: tx,
        }))
        .await
        .unwrap();

    let msg = rx.await.unwrap();

    HttpResponse::Ok().body(msg)
}

#[get("/ping/{node_id}")]
async fn ping(
    node_id: web::Path<String>,
    client_sender: web::Data<Sender<Command>>,
) -> impl Responder {
    let node_id = node_id.parse::<NodeId>().unwrap();

    let (tx, rx) = oneshot::channel::<String>();

    client_sender
        .send(Command::Ping(Ping {
            node_id,
            request_id: rand::thread_rng().gen(),
            response: tx,
        }))
        .await
        .unwrap();

    let msg = rx.await.unwrap();

    HttpResponse::Ok().body(msg)
}

async fn receiver_task(client: Client, pings: Pings) {
    let mut receiver = client.forward_receiver().await.unwrap();

    while let Some(fwd) = receiver.recv().await {
        if let Err(e) = handle_forward_message(fwd, &client, &pings).await {
            log::error!("Handle forward message failed: {e}")
        }
    }
}

async fn handle_forward_message(
    fwd: ya_relay_client::Forwarded,
    client: &Client,
    pings: &Pings,
) -> Result<()> {
    log::info!("Got forward message: {fwd:?}");
    match fwd.transport {
        ya_relay_client::TransportType::Reliable => {
            let msg = String::from_utf8(fwd.payload.into_vec())?;

            let mut s = msg.split(':');
            let command = s
                .next()
                .ok_or_else(|| anyhow!("No message command found"))?;
            let request_id = s
                .next()
                .ok_or_else(|| anyhow!("No request ID found"))?
                .parse::<u32>()?;

            match command {
                "Ping" => {
                    let mut sender = client.forward_reliable(fwd.node_id).await?;
                    sender
                        .send(format!("Pong:{request_id}").as_bytes().to_vec().into())
                        .await?;

                    Ok(())
                }
                "Pong" => {
                    if let Some((ts, sender)) = pings.lock().unwrap().remove(&request_id) {
                        sender
                            .send(format!(
                                "Ping node {} took {} ms\n",
                                fwd.node_id,
                                ts.elapsed().as_millis()
                            ))
                            .unwrap();
                    }
                    Ok(())
                }
                other_cmd => Err(anyhow!("Invalid command: {other_cmd}")),
            }
        }
        ya_relay_client::TransportType::Unreliable => Ok(()),
        ya_relay_client::TransportType::Transfer => Ok(()),
    }
}

async fn client_task(client: Client, mut rx: mpsc::Receiver<Command>, pings: Pings) -> Result<()> {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::FindNode(FindNode { node_id, response }) => {
                let now = Instant::now();

                match client.find_node(node_id).await {
                    Ok(node) => {
                        response
                            .send(format!(
                                "Found node {node:?} in {} ms\n",
                                now.elapsed().as_millis()
                            ))
                            .unwrap();
                    }
                    Err(e) => log::error!("Find node failed: {e}"),
                }
            }
            Command::Ping(Ping {
                node_id,
                request_id,
                response,
            }) => {
                //TODO choose which transport type in API
                match client.forward_reliable(node_id).await {
                    Ok(mut sender) => {
                        let msg = format!("Ping:{request_id}");

                        let now = Instant::now();
                        if let Err(e) = sender.send(msg.as_bytes().to_vec().into()).await {
                            log::error!("Send failed: {e}");
                        } else {
                            pings.lock().unwrap().insert(request_id, (now, response));
                        }
                    }
                    Err(e) => log::error!("Forward reliable failed: {e}"),
                }
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
    let client_cloned = client.clone();

    let pings = Pings::default();
    let pings_cloned = pings.clone();

    tokio::task::spawn_local(async move {
        receiver_task(client_cloned, pings_cloned).await;
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
