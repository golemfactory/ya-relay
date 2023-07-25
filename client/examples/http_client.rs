use ya_relay_client::{Client, ClientBuilder, FailFast, GenericSender};
use ya_relay_core::{
    crypto::FallbackCryptoProvider,
    key::{load_or_generate, Protected},
    NodeId,
};

use actix_web::error::{ErrorBadRequest, ErrorInternalServerError};
use actix_web::{
    get,
    web::{self, Data},
    App, HttpResponse, HttpServer, Responder,
};
use anyhow::{anyhow, Result};
use futures::{future, try_join, FutureExt};
use rand::Rng;
use std::time::Duration;
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::Instant,
};
use structopt::StructOpt;
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
use ya_relay_proto::proto::response::Node;
use ya_relay_proto::proto::Protocol;

#[derive(StructOpt)]
struct Cli {
    #[structopt(long, env = "API_PORT")]
    api_port: u16,
    #[structopt(long, env = "P2P_BIND_ADDR")]
    p2p_bind_addr: url::Url,
    #[structopt(long, env = "RELAY_ADDR")]
    relay_addr: url::Url,
    #[structopt(long, env = "KEY_FILE")]
    key_file: Option<String>,
    #[structopt(long, env = "PASSWORD", parse(from_str = Protected::from))]
    password: Option<Protected>,
}

type RequestIdToPingResponseMap = HashMap<u32, (Instant, oneshot::Sender<Result<String, String>>)>;

#[derive(Clone, Default)]
struct Pings {
    inner: Arc<Mutex<RequestIdToPingResponseMap>>,
}

struct RequestGuard {
    inner: Arc<Mutex<RequestIdToPingResponseMap>>,
    id: u32,
    rx: oneshot::Receiver<Result<String, String>>,
}

impl Pings {
    pub fn request(&self) -> RequestGuard {
        let id = rand::thread_rng().gen();
        let inner = self.inner.clone();
        let (tx, rx) = oneshot::channel();

        inner.lock().unwrap().insert(id, (Instant::now(), tx));

        RequestGuard { inner, id, rx }
    }

    pub fn respond(
        &self,
        request_id: u32,
    ) -> Result<(Instant, oneshot::Sender<Result<String, String>>)> {
        self.inner
            .lock()
            .unwrap()
            .remove(&request_id)
            .ok_or_else(|| anyhow!("response to invalid request {}", request_id))
    }
}

impl RequestGuard {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub async fn result(mut self) -> Result<String, String> {
        let rx = &mut self.rx;
        rx.await.map_err(|e| e.to_string())?
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let _ = self.inner.lock().unwrap().remove(&self.id);
    }
}

#[derive(Debug)]
enum Command {
    FindNode(FindNode),
    Ping(Ping),
}

#[derive(Debug)]
struct FindNode {
    node_id: NodeId,
    response: oneshot::Sender<Result<(Node, Duration), String>>,
}

#[derive(Debug)]
struct Ping {
    node_id: NodeId,
    response: oneshot::Sender<Result<String, String>>,
}

struct DisplayableNode(Node);

impl fmt::Display for DisplayableNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identities = self
            .0
            .identities
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let endpoints = self
            .0
            .endpoints
            .iter()
            .map(|ep| {
                format!(
                    "{p}://{ip}:{port}",
                    p = if ep.protocol == i32::from(Protocol::Udp) {
                        "UDP"
                    } else if ep.protocol == i32::from(Protocol::Tcp) {
                        "TCP"
                    } else {
                        "Unknown"
                    },
                    ip = ep.address,
                    port = ep.port
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let slot = self.0.slot;
        let encr = self
            .0
            .supported_encryptions
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        f.write_str(
            format!(
                "Node \
        \n  with Identities: {identities} \
        \n  with Endpoints: {endpoints} \
        \n  with Slot: {slot} \
        \n  with Supported encryption: {encr} \
        \n"
            )
            .as_str(),
        )
    }
}

#[get("/find-node/{node_id}")]
async fn find_node(
    node_id: web::Path<String>,
    client_sender: web::Data<Sender<Command>>,
) -> impl Responder {
    let node_id = node_id.parse::<NodeId>().map_err(ErrorBadRequest)?;

    let (tx, rx) = oneshot::channel();

    client_sender
        .send(Command::FindNode(FindNode {
            node_id,
            response: tx,
        }))
        .await
        .map_err(ErrorInternalServerError)?;

    let msg = rx
        .await
        .map(|rx| match rx {
            Ok((node, time)) => format!(
                "Found {} \nin {} ms",
                DisplayableNode(node),
                time.as_millis()
            ),
            Err(err) => err.to_string(),
        })
        .map_err(ErrorInternalServerError)?;

    println!("{}", msg);

    Ok::<_, actix_web::Error>(HttpResponse::Ok().body(msg))
}

#[get("/ping/{node_id}")]
async fn ping(
    node_id: web::Path<String>,
    client_sender: web::Data<Sender<Command>>,
) -> impl Responder {
    let node_id = node_id.parse::<NodeId>().map_err(ErrorBadRequest)?;

    let (tx, rx) = oneshot::channel();

    client_sender
        .send(Command::Ping(Ping {
            node_id,
            response: tx,
        }))
        .await
        .map_err(ErrorInternalServerError)?;

    let msg = rx
        .await
        .map_err(ErrorInternalServerError)?
        .map_err(ErrorInternalServerError)?;

    Ok::<_, actix_web::Error>(HttpResponse::Ok().body(msg))
}

async fn receiver_task(client: Client, pings: Pings) -> anyhow::Result<()> {
    let mut receiver = client
        .forward_receiver()
        .await
        .ok_or(anyhow!("Couldn't get forward receiver"))?;

    while let Some(fwd) = receiver.recv().await {
        if let Err(e) = handle_forward_message(fwd, &client, &pings).await {
            log::error!("Handle forward message failed: {e}")
        }
    }
    Ok(())
}

async fn handle_forward_message(
    fwd: ya_relay_client::Forwarded,
    client: &Client,
    pings: &Pings,
) -> Result<()> {
    match fwd.transport {
        ya_relay_client::TransportType::Reliable => {
            log::info!("Got forward message: {fwd:?}");
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
                    match pings.respond(request_id) {
                        Ok((ts, sender)) => sender
                            .send(Ok(format!(
                                "Ping node {} took {} ms\n",
                                fwd.node_id,
                                ts.elapsed().as_millis()
                            )))
                            .ok(),
                        Err(e) => {
                            log::warn!("ping: {:?}", e);
                            None
                        }
                    };
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
                        response.send(Ok((node, now.elapsed()))).ok();
                    }
                    Err(e) => {
                        let _ = response.send(Err(format!("Find node failed: {e}")));
                        log::error!("Find node failed: {e}")
                    }
                }
            }
            Command::Ping(Ping { node_id, response }) => {
                log::info!("try ping: {}", node_id);
                match client.forward_reliable(node_id).await {
                    Ok(mut sender) => {
                        let r = pings.request();
                        let msg = format!("Ping:{}", r.id());
                        let _ = if let Err(e) = sender.send(msg.as_bytes().to_vec().into()).await {
                            log::error!("Send failed: {e}");
                            response.send(Err(format!("Send failed: {e}")))
                        } else {
                            response.send(r.result().await)
                        };
                    }
                    Err(e) => {
                        log::error!("Forward reliable failed: {e}");
                        let _ = response.send(Err(format!("Forward reliable failed: {e}")));
                    }
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
    let client = build_client(
        cli.relay_addr,
        cli.p2p_bind_addr,
        cli.key_file.as_deref(),
        cli.password,
    )
    .await?;
    let client_cloned = client.clone();

    let pings = Pings::default();
    let pings_cloned = pings.clone();

    let receiver = receiver_task(client_cloned, pings_cloned);

    let client = client_task(client, rx, pings);

    let port = cli.api_port;

    let http_server = HttpServer::new(move || {
        App::new()
            .app_data(Data::new(tx.clone()))
            .service(find_node)
            .service(ping)
    })
    .workers(4)
    .bind(("0.0.0.0", port))?
    .run();

    let handle = http_server.handle();

    try_join!(
        http_server.then(|_| future::err::<(), anyhow::Error>(anyhow!("stop"))),
        async move {
            try_join!(receiver, client)?;
            log::error!("exit!");
            handle.stop(true).await;
            Ok(())
        },
    )?;

    Ok(())
}

async fn build_client(
    relay_addr: url::Url,
    p2p_bind_addr: url::Url,
    key_file: Option<&str>,
    password: Option<Protected>,
) -> Result<Client> {
    let secret = key_file.map(|key_file| load_or_generate(key_file, password));
    let provider = if let Some(secret_key) = secret {
        FallbackCryptoProvider::new(secret_key)
    } else {
        FallbackCryptoProvider::default()
    };

    let builder = ClientBuilder::from_url(relay_addr)
        .listen(p2p_bind_addr)
        .crypto(provider);

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
