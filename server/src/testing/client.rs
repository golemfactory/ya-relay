use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail};
use bytes::BytesMut;
use ethsign::SecretKey;
use futures::channel::mpsc;
use futures::future::LocalBoxFuture;
use futures::{FutureExt, SinkExt, StreamExt};
use tokio::sync::RwLock;
use url::Url;

use crate::testing::dispatch::{dispatch, Dispatched, Dispatcher, Handler};
use crate::testing::key;
use crate::udp_stream::{udp_bind, OutStream};
use crate::{parse_udp_url, SessionId};

use ya_client_model::NodeId;
use ya_relay_proto::codec;
use ya_relay_proto::proto::{self, Forward, RequestId, SlotId};

pub type ForwardSender = mpsc::Sender<BytesMut>;
pub type ForwardReceiver = mpsc::Receiver<BytesMut>;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(3000);
const SESSION_REQUEST_TIMEOUT: Duration = Duration::from_millis(4000);
const REGISTER_REQUEST_TIMEOUT: Duration = Duration::from_millis(5000);

#[derive(Clone)]
pub struct Client {
    state: Arc<RwLock<ClientState>>,
}

struct ClientState {
    config: ClientConfig,
    sink: Option<OutStream>,
    bind_addr: Option<SocketAddr>,
    sessions: HashMap<SocketAddr, Session>,
    responses: HashMap<SocketAddr, Dispatcher>,
    forward: HashMap<SocketAddr, ForwardSender>,
}

impl ClientState {
    fn new(config: ClientConfig) -> Self {
        Self {
            config,
            sink: Default::default(),
            bind_addr: Default::default(),
            sessions: Default::default(),
            responses: Default::default(),
            forward: Default::default(),
        }
    }

    #[allow(unused)]
    fn remove(&mut self, addr: &SocketAddr) {
        self.sessions.remove(addr);
        self.responses.remove(addr);
        self.forward.remove(addr);
    }

    fn reset(&mut self) {
        *self = Self::new(self.config.clone());
    }
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub bind_url: Url,
    pub srv_addr: SocketAddr,
    pub secret: SecretKey,
    pub auto_connect: bool,
}

#[derive(Clone, Debug)]
pub struct ClientBuilder {
    bind_url: Option<Url>,
    srv_url: Url,
    secret: Option<SecretKey>,
    auto_connect: bool,
}

impl ClientBuilder {
    pub fn from_url(srv_url: Url) -> ClientBuilder {
        ClientBuilder {
            bind_url: None,
            srv_url,
            secret: None,
            auto_connect: false,
        }
    }

    pub fn secret(mut self, secret: SecretKey) -> ClientBuilder {
        self.secret = Some(secret);
        self
    }

    pub fn connect(mut self) -> ClientBuilder {
        self.auto_connect = true;
        self
    }

    pub async fn build(self) -> anyhow::Result<Client> {
        let bind_url = self
            .bind_url
            .unwrap_or_else(|| Url::parse("udp://127.0.0.1:0").unwrap());

        let mut client = Client::new(ClientConfig {
            bind_url,
            srv_addr: parse_udp_url(&self.srv_url)?.parse()?,
            secret: self.secret.unwrap_or_else(key::generate),
            auto_connect: self.auto_connect,
        });

        client.spawn().await?;

        Ok(client)
    }
}

impl Client {
    pub async fn node_id(&self) -> NodeId {
        let state = self.state.read().await;
        NodeId::from(state.config.secret.public().address().clone())
    }

    pub async fn bind_addr(&self) -> anyhow::Result<SocketAddr> {
        self.state
            .read()
            .await
            .bind_addr
            .ok_or_else(|| anyhow!("Client not started"))
    }

    fn new(config: ClientConfig) -> Self {
        let state = Arc::new(RwLock::new(ClientState::new(config)));
        Self { state }
    }

    async fn spawn(&mut self) -> anyhow::Result<()> {
        let (stream, auto_connect) = {
            let mut state = self.state.write().await;
            state.reset();

            let (stream, sink, bind_addr) = udp_bind(&state.config.bind_url).await?;
            state.sink = Some(sink);
            state.bind_addr = Some(bind_addr);

            (stream, state.config.auto_connect)
        };

        tokio::task::spawn_local(dispatch(self.clone(), stream));

        if auto_connect {
            let session = self.server_session().await?;
            session.register_endpoints(vec![]).await?;
        }

        Ok(())
    }
}

impl Client {
    pub async fn server_session(&self) -> anyhow::Result<Session> {
        let addr = self.state.read().await.config.srv_addr;
        Ok(self.session(addr).await?)
    }

    pub async fn session(&self, addr: SocketAddr) -> anyhow::Result<Session> {
        let session = {
            let mut state = self.state.write().await;
            if let Some(session) = state.sessions.get(&addr) {
                return Ok(session.clone());
            }
            state
                .sessions
                .entry(addr)
                .or_insert_with(|| Session::new(addr, self.clone()))
                .clone()
        };
        session.init().await?;
        Ok(session)
    }
}

impl Client {
    async fn init_session(&self, addr: SocketAddr) -> anyhow::Result<SessionId> {
        log::info!("Client: initializing session with {}", addr);

        let response = self
            .request::<proto::response::Challenge>(
                proto::request::Session::default().into(),
                vec![],
                SESSION_REQUEST_TIMEOUT,
                addr,
            )
            .await?;

        // TODO: solve challenge
        // let challenge = response.packet;

        let session_id = SessionId::try_from(response.session_id.clone())?;
        let node_id = self.node_id().await;
        let packet = proto::request::Session {
            challenge_resp: vec![0u8; 2048_usize],
            node_id: node_id.into_array().to_vec(),
            public_key: vec![],
        };

        let response = self
            .request::<proto::response::Session>(
                packet.into(),
                session_id.to_vec(),
                SESSION_REQUEST_TIMEOUT,
                addr,
            )
            .await?;

        if session_id != &response.session_id[..] {
            log::error!(
                "Client: init session id mismatch: {} vs {:?} (response)",
                session_id,
                response.session_id
            )
        }

        {
            let mut state = self.state.write().await;
            let session = state
                .sessions
                .entry(addr)
                .or_insert_with(|| Session::new(addr, self.clone()));
            session.id.write().await.replace(session_id);
        }

        log::info!("Client: session initialized with {}", addr);
        Ok(session_id)
    }

    async fn register_endpoints(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        log::info!("Client: registering endpoints");

        let response = self
            .request::<proto::response::Register>(
                proto::request::Register { endpoints }.into(),
                session_id.to_vec(),
                REGISTER_REQUEST_TIMEOUT,
                addr,
            )
            .await?
            .packet;

        log::info!("Client: registration finished");

        Ok(response.endpoints)
    }

    async fn find_node(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        node_id: NodeId,
    ) -> anyhow::Result<proto::response::Node> {
        let packet = proto::request::Node {
            node_id: node_id.into_array().to_vec(),
            public_key: true,
        };
        let node = self
            .request::<proto::response::Node>(
                packet.into(),
                session_id.to_vec(),
                DEFAULT_REQUEST_TIMEOUT,
                addr,
            )
            .await?
            .packet;

        Ok(node)
    }

    async fn ping(&self, addr: SocketAddr, session_id: SessionId) -> anyhow::Result<()> {
        let packet = proto::request::Ping {};
        self.request::<proto::response::Pong>(
            packet.into(),
            session_id.to_vec(),
            DEFAULT_REQUEST_TIMEOUT,
            addr,
        )
        .await?;

        Ok(())
    }
}

impl Client {
    async fn request<T>(
        &self,
        request: proto::Request,
        session_id: Vec<u8>,
        timeout: Duration,
        addr: SocketAddr,
    ) -> anyhow::Result<Dispatched<T>>
    where
        proto::response::Kind: TryInto<T, Error = ()>,
        T: 'static,
    {
        let request_id = request.request_id;
        let response = self.response::<T>(request_id, timeout, addr).await;

        let packet = proto::Packet {
            session_id,
            kind: Some(proto::packet::Kind::Request(request)),
        };
        self.send(packet, addr).await?;

        Ok(response.await?)
    }

    #[inline(always)]
    async fn response<'a, T>(
        &self,
        request_id: RequestId,
        timeout: Duration,
        addr: SocketAddr,
    ) -> LocalBoxFuture<'a, anyhow::Result<Dispatched<T>>>
    where
        proto::response::Kind: TryInto<T, Error = ()>,
        T: 'static,
    {
        let dispatcher = {
            let mut state = self.state.write().await;
            (*state).responses.entry(addr).or_default().clone()
        };
        dispatcher.response::<T>(request_id, timeout)
    }

    async fn send(
        &self,
        packet: impl Into<codec::PacketKind>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let mut sink = {
            let mut state = self.state.write().await;
            match state.sink {
                Some(ref mut sink) => sink.clone(),
                None => bail!("Not connected"),
            }
        };
        Ok(sink.send((packet.into(), addr)).await?)
    }
}

impl Handler for Client {
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Dispatcher>> {
        let handler = self.clone();
        async move {
            let state = handler.state.read().await;
            state.responses.get(&from).cloned()
        }
        .boxed_local()
    }

    fn on_control(
        &self,
        _session_id: Vec<u8>,
        control: proto::Control,
        from: SocketAddr,
    ) -> LocalBoxFuture<()> {
        log::debug!(
            "Client: received control packet from {}: {:?}",
            from,
            control
        );
        Box::pin(futures::future::ready(()))
    }

    fn on_request(
        &self,
        session_id: Vec<u8>,
        request: proto::Request,
        from: SocketAddr,
    ) -> LocalBoxFuture<()> {
        log::debug!(
            "Client: received request packet from {}: {:?}",
            from,
            request
        );

        if let proto::Request {
            request_id,
            kind: Some(kind),
        } = request
        {
            match kind {
                proto::request::Kind::Ping(_) => {
                    let packet = proto::Packet::response(
                        request_id,
                        session_id,
                        proto::StatusCode::Ok,
                        proto::response::Pong {},
                    );

                    let client = self.clone();
                    return async move {
                        if let Err(e) = client.send(packet, from).await {
                            log::warn!("Client: unable to send Pong to {}: {}", from, e);
                        }
                    }
                    .boxed_local();
                }
                _ => {}
            }
        }

        Box::pin(futures::future::ready(()))
    }

    fn on_forward(&self, forward: proto::Forward, from: SocketAddr) -> LocalBoxFuture<()> {
        let client = self.clone();
        async move {
            match {
                let state = client.state.read().await;
                state.forward.get(&from).cloned()
            } {
                Some(mut sender) => {
                    if let Err(_) = sender.send(forward.payload).await {
                        log::debug!("Unable to forward packet: handler closed");
                    }
                }
                None => log::debug!("No forward handler exists for {}", from),
            }
        }
        .boxed_local()
    }
}

#[derive(Clone)]
pub struct Session {
    pub id: Arc<RwLock<Option<SessionId>>>,
    remote: SocketAddr,
    client: Client,
}

impl Session {
    fn new(remote: SocketAddr, client: Client) -> Self {
        Self {
            id: Default::default(),
            remote,
            client,
        }
    }

    pub async fn id(&self) -> anyhow::Result<SessionId> {
        let id = self.id.read().await;
        id.clone().ok_or_else(|| anyhow!("Not connected"))
    }

    pub async fn init(&self) -> anyhow::Result<SessionId> {
        self.client.init_session(self.remote).await
    }

    pub async fn register_endpoints(
        &self,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        let session_id = self.id().await?;
        self.client
            .register_endpoints(self.remote, session_id, endpoints)
            .await
    }

    pub async fn find_node(&self, node_id: NodeId) -> anyhow::Result<proto::response::Node> {
        let session_id = self.id().await?;
        self.client
            .find_node(self.remote, session_id, node_id)
            .await
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let session_id = self.id().await?;
        self.client.ping(self.remote, session_id).await
    }

    pub async fn forward(
        self,
        slot: SlotId,
        send: ForwardReceiver,
        receive: ForwardSender,
    ) -> anyhow::Result<()> {
        let session_id = self.id().await?;
        let addr = self.remote;
        {
            let mut state = self.client.state.write().await;
            state.forward.insert(addr, receive);
        }

        let client = self.client.clone();
        tokio::task::spawn_local(send.for_each(move |payload| {
            let client = client.clone();
            let forward = Forward {
                session_id: session_id.clone().into(),
                slot,
                payload,
            };

            async move {
                if let Err(e) = client.send(forward, addr).await {
                    log::warn!("Unable to forward packet to {}: {}", addr, e);
                }
            }
        }));

        Ok(())
    }
}
