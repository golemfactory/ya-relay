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
use std::{
    collections::HashMap,
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};
use structopt::StructOpt;
use tokio::{io::AsyncReadExt, sync::oneshot};
use ya_relay_proto::proto::response::Node;
use ya_relay_proto::proto::Protocol;

#[path = "http_client/wrap.rs"]
mod wrap;

#[derive(StructOpt)]
struct Cli {
    #[structopt(long, env = "API_PORT")]
    api_port: u16,
    #[structopt(long, env = "P2P_BIND_ADDR")]
    p2p_bind_addr: Option<url::Url>,
    #[structopt(long, env = "RELAY_ADDR")]
    relay_addr: url::Url,
    #[structopt(long, env = "KEY_FILE")]
    key_file: Option<String>,
    #[structopt(long, env = "PASSWORD", parse(from_str = Protected::from))]
    password: Option<Protected>,
}

type ClientWrap = self::wrap::SendWrap<Client>;

type RequestIdToMessageResponse = HashMap<u32, (Instant, oneshot::Sender<Result<String, String>>)>;

#[derive(Clone, Default)]
struct Messages {
    inner: Arc<Mutex<RequestIdToMessageResponse>>,
}

struct RequestGuard {
    inner: Arc<Mutex<RequestIdToMessageResponse>>,
    id: u32,
    rx: oneshot::Receiver<Result<String, String>>,
}

impl Messages {
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
    client_sender: web::Data<ClientWrap>,
) -> impl Responder {
    let node_id = node_id.parse::<NodeId>().map_err(ErrorBadRequest)?;
    let (node, time) = client_sender
        .run_async(move |client: Client| async move {
            let now = Instant::now();
            match client.find_node(node_id).await {
                Ok(node) => Ok((node, now.elapsed())),
                Err(e) => {
                    log::error!("Find node failed: {e}");
                    Err(format!("Find node failed: {e}"))
                }
            }
        })
        .await
        .map_err(ErrorInternalServerError)?
        .map_err(ErrorInternalServerError)?;

    let msg = format!(
        "Found {} \nin {} ms",
        DisplayableNode(node),
        time.as_millis()
    );

    log::debug!("[find-node]: {}", msg);

    Ok::<_, actix_web::Error>(HttpResponse::Ok().body(msg))
}

#[get("/ping/{node_id}")]
async fn ping(
    node_id: web::Path<NodeId>,
    client_sender: web::Data<ClientWrap>,
    messages: web::Data<Messages>,
) -> impl Responder {
    let node_id = node_id.into_inner();
    let msg = client_sender
        .run_async(move |client: Client| async move {
            match client.forward_reliable(node_id).await {
                Ok(mut sender) => {
                    let r = messages.request();
                    let msg = format!("Ping:{}", r.id());
                    if let Err(e) = sender.send(msg.as_bytes().to_vec().into()).await {
                        log::error!("Send failed: {e}");
                        Err(format!("Send failed: {e}"))
                    } else {
                        r.result().await
                    }
                }
                Err(e) => {
                    log::error!("Forward reliable failed: {e}");
                    Err(format!("Forward reliable failed: {e}"))
                }
            }
        })
        .await
        .map_err(ErrorInternalServerError)?
        .map_err(ErrorInternalServerError)?;

    log::debug!("[ping]: {}", msg);

    Ok::<_, actix_web::Error>(HttpResponse::Ok().body(msg))
}

#[get("/transfer-file/{node_id}/{chunk_size_kb}/{filename}")]
async fn transfer_file(
    path: web::Path<(NodeId, usize, PathBuf)>,
    client_sender: web::Data<ClientWrap>,
    messages: web::Data<Messages>,
) -> impl Responder {
    let node_id = path.0;
    let chunk_size_bytes = path.1 * 1024;
    let filename = path.2.clone();

    let msg = client_sender
        .run_async(move |client: Client| async move {
            match tokio::fs::File::open(filename).await {
                Ok(mut file) => {
                    let mut data = vec![];
                    file.read_to_end(&mut data).await.unwrap();

                    let r = messages.request();
                    let chunks_quantity = data.chunks(chunk_size_bytes).count();
                    let start_msg = format!("Transfer:{}:{}", r.id(), chunks_quantity);
                    match client.forward_reliable(node_id).await {
                        Ok(mut sender) => {
                            sender
                                .send(start_msg.as_bytes().to_vec().into())
                                .await
                                .map_err(|e| format!("Send failed: {e}"))?;

                            for chunk in data.chunks(chunk_size_bytes) {
                                let msg =
                                    format!("T:{}:{}", r.id(), std::str::from_utf8(chunk).unwrap());
                                log::info!("Sending: {msg}");
                                sender
                                    .send(msg.as_bytes().to_vec().into())
                                    .await
                                    .map_err(|e| format!("Send failed: {e}"))?;
                            }
                            r.result().await
                        }
                        Err(e) => {
                            log::error!("Transfer file failed: {e}");
                            Err(format!("Transfer file failed: {e}"))
                        }
                    }
                }
                Err(e) => {
                    log::error!("Transfer file failed: {e}");
                    Err(format!("Transfer file failed: {e}"))
                }
            }
        })
        .await
        .map_err(ErrorInternalServerError)?
        .map_err(ErrorInternalServerError)?;

    log::debug!("[ping]: {}", msg);

    Ok::<_, actix_web::Error>(HttpResponse::Ok().body(msg))
}

async fn receiver_task(client: Client, messages: Messages) -> anyhow::Result<()> {
    let mut receiver = client
        .forward_receiver()
        .await
        .ok_or(anyhow!("Couldn't get forward receiver"))?;

    while let Some(fwd) = receiver.recv().await {
        let mut transfers: HashMap<u32, (usize, usize)> = HashMap::default();
        if let Err(e) = handle_forward_message(fwd, &client, &messages, &mut transfers).await {
            log::error!("Handle forward message failed: {e}")
        }
    }
    Ok(())
}

async fn handle_forward_message(
    fwd: ya_relay_client::channels::Forwarded,
    client: &Client,
    messages: &Messages,
    transfers: &mut HashMap<u32, (usize, usize)>,
) -> Result<()> {
    match fwd.transport {
        ya_relay_client::model::TransportType::Reliable => {
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
                    match messages.respond(request_id) {
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
                "Transfer" => {
                    let chunks_quantity = s
                        .next()
                        .ok_or_else(|| anyhow!("No chunks quantity found"))?
                        .parse::<usize>()?;

                    transfers.insert(request_id, (chunks_quantity, 0));

                    Ok(())
                }
                "T" => {
                    log::info!("GOT T");
                    if let Some((chunks_quantity, counter)) = transfers.get_mut(&request_id) {
                        *counter += 1;

                        if counter == chunks_quantity {
                            let mut sender = client.forward_reliable(fwd.node_id).await?;
                            sender
                                .send(
                                    format!("TransferResponse:{request_id}:{chunks_quantity}")
                                        .as_bytes()
                                        .to_vec()
                                        .into(),
                                )
                                .await?;

                            transfers.remove(&request_id);
                        }
                    }

                    Ok(())
                }
                "TransferResponse" => {
                    match messages.respond(request_id) {
                        Ok((ts, sender)) => {
                            let bytes_transferred = s
                                .next()
                                .ok_or_else(|| anyhow!("No bytes_transferred found"))?
                                .parse::<usize>()?;

                            sender
                                .send(Ok(format!(
                                    "Transfer of {} MB to node {} took {} ms which is {} MB/s\n",
                                    bytes_transferred / (1024 * 1024),
                                    fwd.node_id,
                                    ts.elapsed().as_millis(),
                                    (bytes_transferred / (1024 * 1024)) as f32
                                        / ts.elapsed().as_secs_f32()
                                )))
                                .ok()
                        }
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
        ya_relay_client::model::TransportType::Unreliable => Ok(()),
        ya_relay_client::model::TransportType::Transfer => Ok(()),
    }
}

async fn run() -> Result<()> {
    env_logger::init();

    let cli = Cli::from_args();
    let client = build_client(
        cli.relay_addr,
        cli.p2p_bind_addr,
        cli.key_file.as_deref(),
        cli.password,
    )
    .await?;
    let client_cloned = client.clone();

    let messages = Messages::default();
    let messages_cloned = messages.clone();

    let receiver = receiver_task(client_cloned, messages_cloned);

    let client = Data::new(wrap::wrap(client));
    let web_messages = Data::new(messages);

    let port = cli.api_port;

    let http_server = HttpServer::new(move || {
        App::new()
            .app_data(client.clone())
            .app_data(web_messages.clone())
            .service(find_node)
            .service(ping)
            .service(transfer_file)
    })
    .workers(4)
    .bind(("0.0.0.0", port))?
    .run();

    let handle = http_server.handle();

    try_join!(
        http_server.then(|_| future::err::<(), anyhow::Error>(anyhow!("stop"))),
        async move {
            try_join!(receiver)?;
            log::error!("exit!");
            handle.stop(true).await;
            Ok(())
        },
    )?;

    Ok(())
}

async fn build_client(
    relay_addr: url::Url,
    p2p_bind_addr: Option<url::Url>,
    key_file: Option<&str>,
    password: Option<Protected>,
) -> Result<Client> {
    let secret = key_file.map(|key_file| load_or_generate(key_file, password));
    let provider = if let Some(secret_key) = secret {
        FallbackCryptoProvider::new(secret_key)
    } else {
        FallbackCryptoProvider::default()
    };

    let mut builder = ClientBuilder::from_url(relay_addr).crypto(provider);

    if let Some(bind) = p2p_bind_addr {
        builder = builder.listen(bind);
    }

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
