use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail};
use ethsign::{PublicKey, SecretKey};
use futures::channel::mpsc;
use futures::future::LocalBoxFuture;
use futures::{FutureExt, SinkExt, StreamExt};
use tokio::sync::RwLock;
use url::Url;

use crate::challenge::{Challenge, DefaultChallenge};
use crate::server::Server;
use crate::testing::dispatch::{dispatch, Dispatched, Dispatcher, Handler};
use crate::testing::key;
use crate::udp_stream::{udp_bind, OutStream};
use crate::{parse_udp_url, SessionId};

use ya_client_model::NodeId;
use ya_net_stack::interface::*;
use ya_net_stack::smoltcp::iface::Route;
use ya_net_stack::smoltcp::wire::{IpAddress, IpCidr, IpEndpoint};
use ya_net_stack::socket::SocketEndpoint;
use ya_net_stack::{Channel, IngressEvent, Network, Protocol, Stack};
use ya_relay_proto::codec;
use ya_relay_proto::proto::{self, Forward, Payload, RequestId, SlotId};

pub type ForwardSender = mpsc::Sender<Vec<u8>>;
pub type ForwardReceiver = mpsc::Receiver<(NodeId, Vec<u8>)>;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(3000);
const SESSION_REQUEST_TIMEOUT: Duration = Duration::from_millis(4000);
const REGISTER_REQUEST_TIMEOUT: Duration = Duration::from_millis(5000);

const STACK_POLL_INTERVAL: Duration = Duration::from_millis(2000);
const TCP_CONNECTION_TIMEOUT: Duration = Duration::from_millis(5000);
const TCP_BIND_PORT: u16 = 1;
const IPV6_DEFAULT_CIDR: u8 = 0;

#[derive(Clone)]
pub struct Client {
    state: Arc<RwLock<ClientState>>,
    net: Network,
}

struct ClientState {
    config: ClientConfig,
    sink: Option<OutStream>,
    bind_addr: Option<SocketAddr>,

    sessions: HashMap<SocketAddr, Session>,
    responses: HashMap<SocketAddr, Dispatcher>,

    virt_ingress: Channel<(NodeId, Vec<u8>)>,
    virt_nodes: HashMap<Box<[u8]>, VirtNode>,
    virt_ips: HashMap<SlotId, Box<[u8]>>,
}

impl ClientState {
    fn new(config: ClientConfig) -> Self {
        Self {
            config,
            sink: Default::default(),
            bind_addr: Default::default(),
            sessions: Default::default(),
            responses: Default::default(),
            virt_nodes: Default::default(),
            virt_ips: Default::default(),
            virt_ingress: Default::default(),
        }
    }

    #[allow(unused)]
    fn remove(&mut self, addr: &SocketAddr) {
        self.sessions.remove(addr);
        self.responses.remove(addr);
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
    pub fn from_server(server: &Server) -> ClientBuilder {
        let url = { server.inner.url.clone() };
        ClientBuilder::from_url(url)
    }

    pub fn from_url(url: Url) -> ClientBuilder {
        ClientBuilder {
            bind_url: None,
            srv_url: url,
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
        })?;

        client.spawn().await?;

        Ok(client)
    }
}

impl Client {
    fn new(config: ClientConfig) -> anyhow::Result<Self> {
        let stack = default_network(config.secret.public())?;
        let state = Arc::new(RwLock::new(ClientState::new(config)));

        Ok(Self { state, net: stack })
    }

    pub fn id(&self) -> String {
        self.net.name.as_ref().clone()
    }

    pub async fn node_id(&self) -> NodeId {
        let state = self.state.read().await;
        NodeId::from(*state.config.secret.public().address())
    }

    pub async fn bind_addr(&self) -> anyhow::Result<SocketAddr> {
        self.state
            .read()
            .await
            .bind_addr
            .ok_or_else(|| anyhow!("client not started"))
    }

    pub async fn forward_receiver(&self) -> Option<ForwardReceiver> {
        let state = self.state.read().await;
        state.virt_ingress.receiver()
    }

    async fn spawn(&mut self) -> anyhow::Result<()> {
        log::debug!("[{}] starting...", self.id());

        let (stream, auto_connect) = {
            let mut state = self.state.write().await;
            state.reset();

            let (stream, sink, bind_addr) = udp_bind(&state.config.bind_url).await?;
            state.sink = Some(sink);
            state.bind_addr = Some(bind_addr);
            (stream, state.config.auto_connect)
        };

        let node_id = self.node_id().await;
        let virt_endpoint: IpEndpoint = (to_ipv6(&node_id), TCP_BIND_PORT).into();

        self.net.bind(Protocol::Tcp, virt_endpoint)?;

        self.spawn_ingress_router()?;
        self.spawn_egress_router()?;
        self.spawn_stack_poller();

        tokio::task::spawn_local(dispatch(self.clone(), stream));

        if auto_connect {
            let session = self.server_session().await?;
            session.register_endpoints(vec![]).await?;
        }

        log::debug!("[{}] started", self.id());
        Ok(())
    }

    fn spawn_ingress_router(&self) -> anyhow::Result<()> {
        let ingress_rx = self
            .net
            .ingress_receiver()
            .ok_or_else(|| anyhow::anyhow!("ingress traffic router already spawned"))?;

        let client = self.clone();
        tokio::task::spawn_local(ingress_rx.for_each(move |event| {
            let client = client.clone();
            async move {
                let (desc, payload) = match event {
                    IngressEvent::InboundConnection { desc } => {
                        log::trace!(
                            "[{}] ingress router: new connection from {:?} to {:?} ",
                            client.id(),
                            desc.remote,
                            desc.local,
                        );
                        return;
                    }
                    IngressEvent::Disconnected { desc } => {
                        log::trace!(
                            "[{}] ingress router: {:?} disconnected from {:?}",
                            client.id(),
                            desc.remote,
                            desc.local,
                        );
                        return;
                    }
                    IngressEvent::Packet { desc, payload } => (desc, payload),
                };
                let remote_address = match desc.remote {
                    SocketEndpoint::Ip(endpoint) => endpoint.addr,
                    _ => {
                        log::trace!(
                            "[{}] ingress router: remote endpoint {:?} is not supported",
                            client.id(),
                            desc.remote
                        );
                        return;
                    }
                };
                match {
                    // nodes are populated via `Client::on_forward` and `Client::forward`
                    let state = client.state.read().await;
                    state
                        .virt_nodes
                        .get(remote_address.as_bytes())
                        .map(|node| (node.id, state.virt_ingress.tx.clone()))
                } {
                    Some((node_id, mut tx)) => {
                        if tx.send((node_id, payload)).await.is_err() {
                            log::trace!(
                                "[{}] ingress router: ingress handler closed for node {}",
                                client.id(),
                                node_id
                            );
                        }
                    }
                    _ => log::trace!(
                        "[{}] ingress router: unknown remote address {}",
                        client.id(),
                        remote_address
                    ),
                };
            }
        }));

        Ok(())
    }

    fn spawn_egress_router(&self) -> anyhow::Result<()> {
        let egress_rx = self
            .net
            .egress_receiver()
            .ok_or_else(|| anyhow::anyhow!("egress traffic router already spawned"))?;

        let client = self.clone();
        tokio::task::spawn_local(egress_rx.for_each(move |egress| {
            let client = client.clone();
            async move {
                let node = match {
                    let state = client.state.read().await;
                    state.virt_nodes.get(&egress.remote).cloned()
                } {
                    Some(node) => node,
                    None => {
                        log::trace!(
                            "[{}] egress router: unknown address {:02x?}",
                            client.id(),
                            egress.remote
                        );
                        return;
                    }
                };

                let forward = Forward {
                    session_id: node.session_id.into(),
                    slot: node.session_slot,
                    payload: Payload::from(egress.payload),
                };

                if let Err(error) = client.send(forward, node.session_addr).await {
                    log::trace!(
                        "[{}] egress router: forward to {} failed: {}",
                        client.id(),
                        node.session_addr,
                        error
                    );
                }
            }
        }));

        Ok(())
    }

    fn spawn_stack_poller(&self) {
        let net = self.net.clone();
        tokio::task::spawn_local(async move {
            loop {
                tokio::time::delay_for(STACK_POLL_INTERVAL).await;
                net.poll();
            }
        });
    }

    async fn resolve_slot(&self, slot: SlotId, addr: SocketAddr) -> anyhow::Result<VirtNode> {
        match self.get_slot(&slot).await {
            Some(node) => Ok(node),
            None => match async {
                let session = self.session(addr).await?;
                session.find_slot(slot).await?;
                Ok::<_, anyhow::Error>(self.get_slot(&slot).await)
            }
            .await
            {
                Ok(Some(node)) => Ok(node),
                Ok(None) => anyhow::bail!("empty node response"),
                Err(err) => anyhow::bail!("slot resolution error: {}", err),
            },
        }
    }

    async fn get_slot(&self, slot: &SlotId) -> Option<VirtNode> {
        let state = self.state.read().await;
        state
            .virt_ips
            .get(slot)
            .map(|ip| state.virt_nodes.get(ip).cloned())
            .flatten()
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
        let id = self.id();
        log::info!("[{}] initializing session with {}", id, addr);

        let response = self
            .request::<proto::response::Challenge>(
                proto::request::Session::default().into(),
                vec![],
                SESSION_REQUEST_TIMEOUT,
                addr,
            )
            .await?;

        let packet = response.packet;
        let secret_key = self.state.read().await.config.secret.clone();
        let public_key = secret_key.public();
        let challenge_resp = DefaultChallenge::with(secret_key)
            .solve(packet.challenge.as_slice(), packet.difficulty)?;

        let session_id = SessionId::try_from(response.session_id.clone())?;
        let node_id = self.node_id().await;

        let packet = proto::request::Session {
            challenge_resp,
            node_id: node_id.into_array().to_vec(),
            public_key: public_key.bytes().to_vec(),
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
                "[{}] init session id mismatch: {} vs {:?} (response)",
                id,
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
            let mut state = session.state.write().await;
            state.replace(SessionState { id: session_id });
        }

        log::info!("[{}] session initialized with {}", id, addr);
        Ok(session_id)
    }

    async fn register_endpoints(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        let id = self.id();
        log::info!("[{}] registering endpoints", id);

        let response = self
            .request::<proto::response::Register>(
                proto::request::Register { endpoints }.into(),
                session_id.to_vec(),
                REGISTER_REQUEST_TIMEOUT,
                addr,
            )
            .await?
            .packet;

        log::info!("[{}] registration finished", id);

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
        self.find_node_by(addr, session_id, packet).await
    }

    async fn find_node_by_slot(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        slot: SlotId,
    ) -> anyhow::Result<proto::response::Node> {
        let packet = proto::request::Slot {
            slot,
            public_key: true,
        };
        self.find_node_by(addr, session_id, packet).await
    }

    async fn find_node_by(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        packet: impl Into<proto::Request>,
    ) -> anyhow::Result<proto::response::Node> {
        let response = self
            .request::<proto::response::Node>(
                packet.into(),
                session_id.to_vec(),
                DEFAULT_REQUEST_TIMEOUT,
                addr,
            )
            .await?
            .packet;

        let node = VirtNode::try_new(&response.node_id, session_id, addr, response.slot)?;
        {
            let mut state = self.state.write().await;
            let ip: Box<[u8]> = node.endpoint.addr.as_bytes().into();
            state.virt_nodes.insert(ip.clone(), node);
            state.virt_ips.insert(node.session_slot, ip);
        }

        Ok(response)
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

    async fn forward(
        self,
        session_addr: SocketAddr,
        slot: SlotId,
    ) -> anyhow::Result<ForwardSender> {
        let node = self.resolve_slot(slot, session_addr).await?;
        let connection = self
            .net
            .connect(node.endpoint, TCP_CONNECTION_TIMEOUT)
            .await?;

        let (tx, mut rx) = mpsc::channel(1);
        let client = self.clone();
        let id = client.id();

        tokio::task::spawn_local(async move {
            while let Some(payload) = rx.next().await {
                match client.net.send(payload, connection).await {
                    Ok(_) => client.net.poll(),
                    Err(e) => {
                        log::warn!("[{}] unable to forward via {}: {}", id, session_addr, e);
                    }
                }
            }
            rx.close();

            log::trace!(
                "[{}] forward: disconnected from server: {}",
                id,
                session_addr
            );
        });

        Ok(tx)
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
        let response = self.response::<T>(request.request_id, timeout, addr).await;
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
            let state = self.state.read().await;
            match state.sink {
                Some(ref sink) => sink.clone(),
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
        log::debug!("received control packet from {}: {:?}", from, control);
        Box::pin(futures::future::ready(()))
    }

    fn on_request(
        &self,
        session_id: Vec<u8>,
        request: proto::Request,
        from: SocketAddr,
    ) -> LocalBoxFuture<()> {
        log::debug!("received request packet from {}: {:?}", from, request);

        if let proto::Request {
            request_id,
            kind: Some(proto::request::Kind::Ping(_)),
        } = request
        {
            let packet = proto::Packet::response(
                request_id,
                session_id,
                proto::StatusCode::Ok,
                proto::response::Pong {},
            );

            let client = self.clone();
            return async move {
                if let Err(e) = client.send(packet, from).await {
                    log::warn!("unable to send Pong to {}: {}", from, e);
                }
            }
            .boxed_local();
        }

        Box::pin(futures::future::ready(()))
    }

    fn on_forward(&self, forward: proto::Forward, from: SocketAddr) -> LocalBoxFuture<()> {
        let client = self.clone();
        let fut = async move {
            log::trace!("[{}] received forward packet via {}", client.id(), from);

            if let Err(err) = client.resolve_slot(forward.slot, from).await {
                log::error!("[{}] on forward error: {}", client.id(), err);
                return;
            };

            client.net.receive(forward.payload.into_vec());
            client.net.poll();
        };

        tokio::task::spawn_local(fut);
        futures::future::ready(()).boxed_local()
    }
}

#[derive(Copy, Clone, Debug)]
pub struct VirtNode {
    id: NodeId,
    endpoint: IpEndpoint,
    session_id: SessionId,
    session_addr: SocketAddr,
    session_slot: SlotId,
}

impl VirtNode {
    pub fn try_new(
        id: &[u8],
        session_id: SessionId,
        session_addr: SocketAddr,
        session_slot: SlotId,
    ) -> anyhow::Result<Self> {
        let default_id = NodeId::default();
        if id.len() != default_id.as_ref().len() {
            anyhow::bail!("invalid NodeId");
        }

        let id = NodeId::from(id);
        let ip = IpAddress::from(to_ipv6(&id));
        let endpoint = (ip, TCP_BIND_PORT).into();

        Ok(Self {
            id,
            endpoint,
            session_id,
            session_addr,
            session_slot,
        })
    }
}

#[derive(Clone)]
pub struct Session {
    remote_addr: SocketAddr,
    client: Client,
    pub state: Arc<RwLock<Option<SessionState>>>,
}

#[derive(Clone, Copy)]
pub struct SessionState {
    pub id: SessionId,
}

impl Session {
    fn new(remote_addr: SocketAddr, client: Client) -> Self {
        Self {
            client,
            remote_addr,
            state: Default::default(),
        }
    }

    async fn state(&self) -> anyhow::Result<SessionState> {
        self.state
            .read()
            .await
            .ok_or_else(|| anyhow!("Not connected"))
    }

    pub async fn id(&self) -> anyhow::Result<SessionId> {
        let state = self.state.read().await;
        Ok((*state).ok_or_else(|| anyhow!("Not connected"))?.id)
    }

    pub async fn init(&self) -> anyhow::Result<SessionId> {
        self.client.init_session(self.remote_addr).await
    }

    pub async fn close(self) {
        // remove IP entries used in forwarding ?
        self.client.state.write().await.remove(&self.remote_addr);
    }

    pub async fn register_endpoints(
        &self,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        let session_id = self.id().await?;
        self.client
            .register_endpoints(self.remote_addr, session_id, endpoints)
            .await
    }

    pub async fn find_node(&self, node_id: NodeId) -> anyhow::Result<proto::response::Node> {
        let session_id = self.id().await?;
        self.client
            .find_node(self.remote_addr, session_id, node_id)
            .await
    }

    pub async fn find_slot(&self, slot: SlotId) -> anyhow::Result<proto::response::Node> {
        let session_id = self.id().await?;
        self.client
            .find_node_by_slot(self.remote_addr, session_id, slot)
            .await
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let session_id = self.id().await?;
        self.client.ping(self.remote_addr, session_id).await
    }

    pub async fn forward(self, slot: SlotId) -> anyhow::Result<ForwardSender> {
        let _ = { self.state().await?.id };
        self.client.forward(self.remote_addr, slot).await
    }
}

fn default_network(key: PublicKey) -> anyhow::Result<Network> {
    let address = key.address();
    let ipv6_addr = to_ipv6(address);
    let ipv6_cidr = IpCidr::new(IpAddress::from(ipv6_addr), IPV6_DEFAULT_CIDR);
    let mut iface = default_iface();

    let name = format!(
        "{:02x}{:02x}{:02x}{:02x}",
        address[0], address[1], address[2], address[3]
    );

    log::debug!("[{}] Ethernet address: {}", name, iface.ethernet_addr());
    log::debug!("[{}] IP address: {}", name, ipv6_addr);

    add_iface_address(&mut iface, ipv6_cidr);
    add_iface_route(
        &mut iface,
        ipv6_cidr,
        Route::new_ipv6_gateway(ipv6_addr.into()),
    );

    Ok(Network::new(name, Stack::with(iface)))
}

fn to_ipv6(bytes: impl AsRef<[u8]>) -> Ipv6Addr {
    const IPV6_ADDRESS_LEN: usize = 16;

    let bytes = bytes.as_ref();
    let len = IPV6_ADDRESS_LEN.min(bytes.len());
    let mut ipv6_bytes = [0u8; IPV6_ADDRESS_LEN];

    // copy source bytes
    ipv6_bytes[..len].copy_from_slice(&bytes[..len]);
    // no multicast addresses
    ipv6_bytes[0] %= 0xff;
    // no unspecified or localhost addresses
    if ipv6_bytes[0..15] == [0u8; 15] && ipv6_bytes[15] < 0x02 {
        ipv6_bytes[15] = 0x02;
    }

    Ipv6Addr::from(ipv6_bytes)
}
