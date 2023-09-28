use actix_web::{
    error::{ErrorBadRequest, ErrorInternalServerError},
    get, post,
    web::{self, Data},
    App, HttpResponse, HttpServer, Responder,
};
use anyhow::{anyhow, Result};
use chrono::Local;
use futures::{future, try_join, FutureExt};
use rand::Rng;
use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex},
    time::Instant,
};
use std::{collections::VecDeque, time::Duration};
use structopt::StructOpt;
use tokio::sync::oneshot;
use ya_relay_client::{channels::ForwardSender, Client, ClientBuilder, FailFast, GenericSender};
use ya_relay_core::{
    crypto::FallbackCryptoProvider,
    key::{load_or_generate, Protected},
    server_session::TransportType,
    NodeId,
};

use crate::response::{Info, Pong, Transfer};

#[path = "http_client/response.rs"]
mod response;
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
    #[structopt(long, env = "SESSION_EXPIRATION", default_value = "20")]
    session_expiration: u64,
    #[structopt(long, env = "SESSION_REQUEST_TIMEOUT", default_value = "3")]
    session_request_timeout: u64,
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

#[get("/find-node/{node_id}")]
async fn find_node(
    node_id: web::Path<String>,
    client_sender: web::Data<ClientWrap>,
) -> actix_web::Result<HttpResponse> {
    let node_id = node_id.parse::<NodeId>().map_err(ErrorBadRequest)?;
    let (node, duration) = client_sender
        .run_async(move |client: Client| async move {
            let now = Instant::now();
            let node = client.find_node(node_id).await?;

            Ok::<_, anyhow::Error>((node, now.elapsed()))
        })
        .await
        .map_err(|e| {
            log::error!("Run async failed {e}");
            ErrorInternalServerError(e)
        })?
        .map_err(|e| {
            log::error!("Find node failed {e}");
            ErrorInternalServerError(e)
        })?;

    let node = response::Node(node);
    let msg = response::FindNode { node, duration };
    log::debug!("[find-node]: {}", msg);
    Ok::<_, actix_web::Error>(HttpResponse::Ok().json(msg))
}

#[get("/ping/{transport}/{node_id}")]
async fn ping(
    path: web::Path<(TransportType, NodeId)>,
    client_sender: web::Data<ClientWrap>,
    messages: web::Data<Messages>,
) -> actix_web::Result<HttpResponse> {
    let (transport, node_id) = path.into_inner();
    log::trace!("[ping]: Pinging {}", node_id);
    let msg = client_sender
        .run_async(move |client: Client| async move {
            let r = messages.request();
            let mut sender = forward(&client, transport, node_id).await?;
            let msg = format!("Ping:{};", r.id());

            sender.send(msg.as_bytes().to_vec().into()).await?;
            log::trace!("[ping]: Ping {} sent {}. Waiting.", transport, node_id);
            r.result().await.map_err(|e| anyhow!("{e}"))
        })
        .await
        .map_err(|e| {
            log::error!("Run async failed {e}");
            ErrorInternalServerError(e)
        })?
        .map_err(|e| {
            log::error!("Ping failed {e}");
            ErrorInternalServerError(e)
        })?;
    log::debug!("[ping]: {}", msg);
    response::ok_json::<Pong>(&msg)
}

#[get("/disconnect/{node_id}")]
async fn disconnect(
    node_id: web::Path<NodeId>,
    client_sender: web::Data<ClientWrap>,
    messages: web::Data<Messages>,
) -> actix_web::Result<HttpResponse> {
    log::trace!("[disconnect]: Disconnecting {}", node_id);
    let node_id = node_id.into_inner();

    let msg = client_sender
        .run_async(move |client: Client| async move {
            let r = messages.request();
            let msg = format!("Close Session:{}", r.id());
            log::debug!("Sending close session message: {}", msg);
            let result = client.disconnect(node_id).await.map_err(|e| anyhow!("{e}"));
            match result {
                Ok(_) => Ok("Disconnected"),
                Err(e) => Err(e),
            }
        })
        .await
        .map_err(|e| {
            log::error!("Run async failed {e}");
            ErrorInternalServerError(e)
        })?
        .map_err(|e| {
            log::error!("Disconnect failed {e}");
            ErrorInternalServerError(e)
        })?;
    log::debug!("[disconnect]: {}", msg);
    Ok::<_, actix_web::Error>(HttpResponse::Ok().json(msg))
}

#[get("/sessions")]
async fn sessions(client_sender: web::Data<ClientWrap>) -> impl Responder {
    let msg = client_sender
        .run_async(move |client: Client| async move {
            client.sessions().map(response::Sessions::from).await
        })
        .await
        .map_err(ErrorInternalServerError)?;
    Ok::<_, actix_web::Error>(HttpResponse::Ok().json(msg))
}

#[get("/info")]
async fn info(client_sender: web::Data<ClientWrap>) -> impl Responder {
    let msg = client_sender
        .run_async(move |client: Client| async move {
            let node_id = client.node_id();
            let bind_addr = client.bind_addr().await;
            let public_addr = client.public_addr().await;
            Info {
                node_id,
                bind_address: format!("{:?}", bind_addr),
                pub_address: format!("{:?}", public_addr),
            }
        })
        .await
        .map_err(ErrorInternalServerError)?;
    Ok::<_, actix_web::Error>(HttpResponse::Ok().json(msg))
}

#[post("/transfer-file/{transport}/{node_id}")]
async fn transfer_file(
    path: web::Path<(TransportType, NodeId)>,
    client_sender: web::Data<ClientWrap>,
    messages: web::Data<Messages>,
    body: web::Bytes,
) -> actix_web::Result<HttpResponse> {
    let (transport, node_id) = path.into_inner();
    let msg = client_sender
        .run_async(move |client: Client| async move {
            let data: Vec<u8> = body.into();
            let data = String::from_utf8(data)?;

            let r = messages.request();
            let message = format!("Transfer:{}:{}:{};", r.id(), data.len(), data);
            log::trace!("Sending transfer msg: {message:.100}");
            let mut sender = forward(&client, transport, node_id).await?;

            sender.send(message.as_bytes().to_vec().into()).await?;

            r.result().await.map_err(|e| anyhow!("{e}"))
        })
        .await
        .map_err(|e| {
            log::error!("Run async failed {e}");
            ErrorInternalServerError(e)
        })?
        .map_err(|e| {
            log::error!("Transfer file failed {e}");
            ErrorInternalServerError(e)
        })?;
    log::debug!("[transfer-file]: {}", msg);
    response::ok_json::<response::Transfer>(&msg)
}

type PartialMessages = HashMap<NodeId, String>;

const MSG_SEPARATOR: &str = ";";

async fn receiver_task(client: Client, messages: Messages) -> anyhow::Result<()> {
    let mut receiver = client
        .forward_receiver()
        .await
        .ok_or(anyhow!("Couldn't get forward receiver"))?;
    let mut partial_messages = Default::default();
    while let Some(fwd) = receiver.recv().await {
        if let Err(e) =
            handle_forward_messages(fwd, &client, &messages, &mut partial_messages).await
        {
            log::warn!("Handle forward message failed: {e}")
        }
    }
    Ok(())
}

async fn handle_forward_messages(
    fwd: ya_relay_client::channels::Forwarded,
    client: &Client,
    messages: &Messages,
    partial_messages: &mut PartialMessages,
) -> Result<()> {
    log::info!(
        "Got forward message. Node {}. Transport {}",
        fwd.node_id,
        fwd.transport
    );

    let msgs = String::from_utf8(fwd.payload.into_vec())?
        .trim()
        .to_string();

    let msgs = split_messages(msgs, fwd.node_id, partial_messages)?;

    for msg in msgs {
        handle_forward_message(&msg, fwd.transport, fwd.node_id, client, messages).await?;
    }
    Ok(())
}

fn split_messages(
    msgs: String,
    node_id: NodeId,
    partial_messages: &mut PartialMessages,
) -> Result<VecDeque<String>> {
    let msgs = msgs.trim();
    let previous_last_is_complete = msgs.starts_with(MSG_SEPARATOR);
    let current_last_is_complete = msgs.ends_with(MSG_SEPARATOR);

    let mut msgs = msgs
        .split(MSG_SEPARATOR)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .collect::<VecDeque<String>>();

    if let Some(previous_last_partial) = partial_messages.remove(&node_id) {
        if previous_last_is_complete {
            log::trace!("Pending partial message is complete. Msg: {previous_last_partial:.100} (first 100)");
            msgs.push_front(previous_last_partial);
        } else if let Some(continuation) = msgs.pop_front() {
            let partial_message_continued = format!("{previous_last_partial}{continuation}");
            log::trace!("Merging previous last partial message with first current message. New msg: {partial_message_continued:.100} (first 100)");
            msgs.push_front(partial_message_continued);
        } else {
            anyhow::bail!(format!("Incoming message with id {node_id} should be a continuation of {previous_last_partial:.100} (first 100)"));
        }
    }

    if !current_last_is_complete {
        if let Some(current_partial) = msgs.pop_back() {
            log::trace!("Adding pending partial message: {current_partial:.100} (first 100)");
            partial_messages.insert(node_id, current_partial);
        }
    }

    Ok(msgs)
}

async fn handle_forward_message(
    msg: &str,
    transport: TransportType,
    node_id: NodeId,
    client: &Client,
    messages: &Messages,
) -> Result<()> {
    let msg = msg.trim();
    if msg.is_empty() {
        return Ok(());
    }
    log::trace!("Forward msg data: {msg:.100} (first 100)");
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
            let mut sender = forward(client, transport, node_id).await?;
            sender
                .send(format!("Pong:{request_id};").as_bytes().to_vec().into())
                .await?;

            Ok(())
        }
        "Pong" => {
            match messages.respond(request_id) {
                Ok((ts, sender)) => sender
                    .send(Ok(serde_json::to_string(&Pong {
                        node_id: node_id.to_string(),
                        duration: ts.elapsed(),
                    })?))
                    .ok(),
                Err(e) => {
                    log::warn!("ping: {:?}", e);
                    None
                }
            };
            Ok(())
        }
        "Transfer" => {
            let mut sender = forward(client, transport, node_id).await?;
            let bytes_transferred = s
                .next()
                .ok_or_else(|| anyhow!("No bytes transferred value"))?
                .parse::<usize>()?;
            let data = s.next().ok_or_else(|| anyhow!("No data value"))?;
            if data.len() != bytes_transferred {
                anyhow::bail!("Expected {} bytes, got {}", bytes_transferred, data.len());
            }

            sender
                .send(
                    format!("TransferResponse:{request_id}:{bytes_transferred};")
                        .as_bytes()
                        .to_vec()
                        .into(),
                )
                .await?;

            Ok(())
        }
        "TransferResponse" => {
            match messages.respond(request_id) {
                Ok((ts, sender)) => {
                    let bytes_transferred = s
                        .next()
                        .ok_or_else(|| anyhow!("No bytes_transferred found"))?
                        .parse::<usize>()?;
                    let mb_transfered = bytes_transferred / (1024 * 1024);

                    sender
                        .send(Ok(serde_json::to_string(&Transfer {
                            mb_transfered,
                            node_id: node_id.to_string(),
                            duration: ts.elapsed(),
                            speed: mb_transfered as f32 / ts.elapsed().as_secs_f32(),
                        })?))
                        .ok()
                }
                Err(e) => {
                    log::warn!("ping: {:?}", e);
                    None
                }
            };
            Ok(())
        }
        other_cmd => Err(anyhow!(
            "Invalid command: {other_cmd:.100} (trimmed to 100)"
        )),
    }
}

async fn forward(
    client: &Client,
    transport: TransportType,
    node_id: NodeId,
) -> Result<ForwardSender> {
    match transport {
        TransportType::Unreliable => client.forward_unreliable(node_id).await,
        TransportType::Reliable => client.forward_reliable(node_id).await,
        TransportType::Transfer => client.forward_transfer(node_id).await,
    }
}

async fn run() -> Result<()> {
    env_logger::Builder::new()
        .parse_default_env()
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {:5} {}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.module_path().unwrap_or("<unnamed>"),
                record.args()
            )
        })
        .init();

    let cli = Cli::from_args();
    let client = build_client(
        cli.relay_addr,
        cli.p2p_bind_addr,
        cli.key_file.as_deref(),
        cli.password,
        Duration::from_secs(cli.session_expiration),
        Duration::from_secs(cli.session_request_timeout),
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
            .app_data(web::PayloadConfig::new(1024 * 1024 * 1024 * 4))
            .service(find_node)
            .service(ping)
            .service(sessions)
            .service(disconnect)
            .service(info)
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
    session_expiration: Duration,
    session_request_timeout: Duration,
) -> Result<Client> {
    let secret = key_file.map(|key_file| load_or_generate(key_file, password));
    let provider = if let Some(secret_key) = secret {
        FallbackCryptoProvider::new(secret_key)
    } else {
        FallbackCryptoProvider::default()
    };

    let mut builder = ClientBuilder::from_url(relay_addr)
        .crypto(provider)
        .expire_session_after(session_expiration)
        .session_request_timeout(session_request_timeout);

    if let Some(bind) = p2p_bind_addr {
        builder = builder.listen(bind);
    }

    let client = builder.connect(FailFast::No).build().await?;

    // This log messages are used by integration tests.
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use crate::split_messages;

    #[test]
    fn complete_1st_partial_2nd_current_no_pending() {
        let msgs = "msg_field_11:msg_field_12;msg_field_21:msg_field_22:msg_field_23".into();
        let node_id = Default::default();
        let partial_messages = &mut Default::default();
        let expected_msgs = VecDeque::from(vec!["msg_field_11:msg_field_12".into()]);
        let expected_partial_messages =
            &mut HashMap::from([(node_id, "msg_field_21:msg_field_22:msg_field_23".into())]);
        assert_eq!(
            expected_msgs,
            split_messages(msgs, node_id, partial_messages).unwrap()
        );
        assert_eq!(expected_partial_messages, partial_messages);
    }

    #[test]
    fn complete_1st_current_no_pending() {
        let msgs = "m1;".into();
        let node_id = Default::default();
        let partial_messages = &mut Default::default();
        let expected_msgs = VecDeque::from(vec!["m1".into()]);
        let expected_partial_messages = &mut HashMap::new();
        assert_eq!(
            expected_msgs,
            split_messages(msgs, node_id, partial_messages).unwrap()
        );
        assert_eq!(expected_partial_messages, partial_messages);
    }

    #[test]
    fn empty_current_no_pending() {
        let msgs = "".into();
        let node_id = Default::default();
        let partial_messages = &mut Default::default();
        let expected_msgs = VecDeque::from(vec![]);
        let expected_partial_messages = &mut HashMap::new();
        assert_eq!(
            expected_msgs,
            split_messages(msgs, node_id, partial_messages).unwrap()
        );
        assert_eq!(expected_partial_messages, partial_messages);
    }

    #[test]
    fn empty_current_and_pending() {
        let msgs = "".into();
        let node_id = Default::default();
        let partial_messages = &mut HashMap::from([(node_id, "x".into())]);
        assert!(split_messages(msgs, node_id, partial_messages).is_err());
    }

    #[test]
    fn empty_current_and_different_pending() {
        let msgs = "".into();
        let node_id = Default::default();
        let partial_messages = &mut HashMap::from([(From::from([1; 20]), "x".into())]);
        let expected_partial_messages = &mut partial_messages.clone();
        split_messages(msgs, node_id, partial_messages).unwrap();
        assert_eq!(expected_partial_messages, partial_messages);
    }

    #[test]
    fn partial_current_and_complete_pending() {
        let msgs = ";y".into();
        let node_id = Default::default();
        let partial_messages = &mut HashMap::from([(node_id, "x".into())]);
        let expected_msgs = VecDeque::from(vec!["x".into()]);
        let expected_partial_messages = &mut HashMap::from([(node_id, "y".into())]);
        assert_eq!(
            expected_msgs,
            split_messages(msgs, node_id, partial_messages).unwrap()
        );
        assert_eq!(expected_partial_messages, partial_messages);
    }

    #[test]
    fn complete_current_and_complete_pending() {
        let msgs = ";y;".into();
        let node_id = Default::default();
        let partial_messages = &mut HashMap::from([(node_id, "x".into())]);
        let expected_msgs = VecDeque::from(vec!["x".into(), "y".into()]);
        let expected_partial_messages = &mut HashMap::new();
        assert_eq!(
            expected_msgs,
            split_messages(msgs, node_id, partial_messages).unwrap()
        );
        assert_eq!(expected_partial_messages, partial_messages);
    }

    #[test]
    fn partial_current_and_partial_pending() {
        let msgs = "y".into();
        let node_id = Default::default();
        let partial_messages = &mut HashMap::from([(node_id, "x".into())]);
        let expected_msgs = VecDeque::new();
        let expected_partial_messages = &mut HashMap::from([(node_id, "xy".into())]);
        assert_eq!(
            expected_msgs,
            split_messages(msgs, node_id, partial_messages).unwrap()
        );
        assert_eq!(expected_partial_messages, partial_messages);
    }

    #[test]
    fn partial_and_complete_current_and_partial_and_different_pending() {
        let msgs = "y;z;q".into();
        let node_id = Default::default();
        let partial_messages =
            &mut HashMap::from([(From::from([2; 20]), "p".into()), (node_id, "x".into())]);
        let expected_msgs = VecDeque::from(vec!["xy".into(), "z".into()]);
        let expected_partial_messages =
            &mut HashMap::from([(From::from([2; 20]), "p".into()), (node_id, "q".into())]);
        assert_eq!(
            expected_msgs,
            split_messages(msgs, node_id, partial_messages).unwrap()
        );
        assert_eq!(expected_partial_messages, partial_messages);
    }
}
