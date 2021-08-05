use anyhow::{anyhow, Context};
use bytes::BytesMut;
use chrono::Utc;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use rand::Rng;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tokio_util::codec::{Decoder, Encoder};
use url::Url;

use crate::error::{
    BadRequest, Error, InternalError, NotFound, ServerResult, Timeout, Unauthorized,
};
use crate::packets::{dispatch_response, PacketsCreator};
use crate::session::{Endpoint, NodeInfo, NodeSession, SessionId};
use crate::udp_stream::{udp_bind, InStream, OutStream};

use crate::state::NodesState;
use ya_client_model::NodeId;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{PacketKind, MAX_PACKET_SIZE};
use ya_relay_proto::proto;
use ya_relay_proto::proto::control::Challenge;
use ya_relay_proto::proto::request::Kind;
use ya_relay_proto::proto::StatusCode;

pub const DEFAULT_NET_PORT: u16 = 7464;
pub const CHALLENGE_SIZE: usize = 16;

#[derive(Clone)]
pub struct Server {
    pub state: Arc<RwLock<ServerState>>,
    pub inner: Arc<ServerImpl>,
}

pub struct ServerState {
    pub nodes: NodesState,
    pub starting_session: HashMap<SessionId, mpsc::Sender<proto::Request>>,

    recv_socket: Option<InStream>,
}

pub struct ServerImpl {
    pub socket: OutStream,
    pub url: Url,
}

impl Server {
    pub async fn dispatch(&self, from: SocketAddr, packet: PacketKind) -> ServerResult<()> {
        match packet {
            PacketKind::Packet(proto::Packet { kind, session_id }) => {
                // Empty `session_id` is sent to initialize Session, but only with Session request.
                if session_id.is_empty() {
                    match kind {
                        Some(proto::packet::Kind::Request(proto::Request {
                            kind: Some(proto::request::Kind::Session(_)),
                        })) => {
                            return self.clone().new_session(from).await;
                        }
                        _ => return Err(BadRequest::NoSessionId.into()),
                    }
                }

                let id = SessionId::try_from(session_id.clone())
                    .map_err(|_| Unauthorized::InvalidSessionId(session_id))?;
                let node = match self.state.read().await.nodes.get_by_session(id) {
                    None => return self.clone().establish_session(id, from, kind).await,
                    Some(node) => node,
                };

                match kind {
                    Some(proto::packet::Kind::Request(proto::Request { kind: Some(kind) })) => {
                        match kind {
                            Kind::Session(_) => {}
                            Kind::Register(_) => {}
                            Kind::Node(params) => self.node_request(id, from, params).await?,
                            Kind::RandomNode(_) => {}
                            Kind::Neighbours(_) => {}
                            Kind::ReverseConn(_) => {}
                            Kind::Ping(_) => self.ping_request(id, from).await?,
                        }
                    }
                    Some(proto::packet::Kind::Response(_)) => {
                        log::warn!(
                            "Server shouldn't get Response packet. Sender: {}, node {}",
                            from,
                            node.info.node_id,
                        );
                    }
                    Some(proto::packet::Kind::Control(_control)) => {
                        log::info!("Control packet from: {}", from);
                    }
                    _ => log::info!("Packet kind: None from: {}", from),
                }
            }
            // TODO: Should we send response for Forward packet??
            PacketKind::Forward(forward) => self.forward(forward, from).await?,
            PacketKind::ForwardCtd(_) => {
                log::info!("ForwardCtd packet from: {}", from)
            }
        };

        Ok(())
    }

    async fn forward(&self, packet: proto::Forward, _from: SocketAddr) -> ServerResult<()> {
        let session_id = SessionId::from(packet.session_id);
        let slot = packet.slot;

        let node = {
            let server = self.state.read().await;

            // Authorization: Sending Node must have established session.
            server
                .nodes
                .get_by_session(session_id)
                .ok_or(Unauthorized::SessionNotFound(session_id.clone()))?;

            server
                .nodes
                .get_by_slot(slot)
                .ok_or(NotFound::NodeBySlot(slot))?
        };

        if node.info.endpoints.len() > 0 {
            // TODO: How to chose best endpoint.
            let endpoint = node.info.endpoints[0].clone();

            self.send_to(PacketKind::ForwardCtd(packet.payload), &endpoint.address)
                .await
                .map_err(|_| InternalError::Send)?;
        } else {
            log::info!(
                "Can't forward packet for session [{}]. Node [{}] has no public address.",
                session_id,
                node.info.node_id
            );
        }

        Ok(())
    }

    async fn public_endpoints(
        &self,
        id: SessionId,
        addr: &SocketAddr,
    ) -> ServerResult<Vec<Endpoint>> {
        // We need new socket to check, if Node's address is public.
        // If we would use the same socket as always, we would be able to
        // send packets even to addresses behind NAT.
        let mut sock = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| InternalError::BindingSocket(e.to_string()))?;

        let mut codec = Codec::default();
        let mut buf = BytesMut::new();

        // If we can ping `from` address it was public, if we don't get response
        // it could be private.
        let ping = PacketKind::ping_packet(id);

        codec
            .encode(ping, &mut buf)
            .map_err(|_| InternalError::Encoding)?;

        sock.send_to(&buf, &addr)
            .await
            .map_err(|_| InternalError::Send)?;

        buf.resize(MAX_PACKET_SIZE as usize, 0);

        let size = timeout(Duration::from_millis(300), sock.recv(&mut buf))
            .await
            .map_err(|_| Timeout::Ping)?
            .map_err(|_| InternalError::Receiving)?;

        buf.truncate(size);

        match dispatch_response(
            codec
                .decode(&mut buf)
                .map_err(|_| InternalError::Decoding)?
                .ok_or(InternalError::Decoding)?,
        ) {
            Ok(proto::response::Kind::Pong(_)) => {}
            _ => return Err(BadRequest::InvalidPacket(id, "Pong".to_string()).into()),
        };

        Ok(vec![Endpoint {
            protocol: proto::Protocol::Udp,
            address: addr.clone(),
        }])
    }

    async fn register_endpoints(
        &self,
        id: SessionId,
        from: SocketAddr,
        _params: proto::request::Register,
        mut node_info: NodeSession,
    ) -> ServerResult<NodeSession> {
        // TODO: Note that we ignore endpoints sent by Node and only try
        //       to verify address, from which we received messages.

        let endpoints = self.public_endpoints(id, &from).await?;

        node_info.info.endpoints.extend(endpoints.into_iter());

        let response = PacketKind::register_response(
            id,
            node_info
                .info
                .endpoints
                .iter()
                .cloned()
                .map(proto::Endpoint::from)
                .collect(),
        );
        self.send_to(response, &from)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!("Responding to Register from: {}", from);
        Ok(node_info)
    }

    async fn ping_request(&self, id: SessionId, from: SocketAddr) -> ServerResult<()> {
        {
            let mut server = self.state.write().await;
            server.nodes.update_seen(id)?;
        }

        self.send_to(PacketKind::pong_response(id), &from)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!("Responding to ping from: {}", from);

        Ok(())
    }

    async fn node_request(
        &self,
        id: SessionId,
        from: SocketAddr,
        params: proto::request::Node,
    ) -> ServerResult<()> {
        if params.node_id.len() != 20 {
            return Err(BadRequest::InvalidNodeId.into());
        }

        let node_id = NodeId::from(&params.node_id[..]);
        let node_info = {
            match self.state.read().await.nodes.get_by_node_id(node_id) {
                None => return Err(NotFound::Node(node_id).into()),
                Some(session) => session.clone(),
            }
        };

        let response = PacketKind::node_response(id, node_info, params.public_key);
        self.send_to(response, &from)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!("Node [{}] info sent to: {}", node_id, from);

        Ok(())
    }

    async fn new_session(self, with: SocketAddr) -> ServerResult<()> {
        let (sender, receiver) = mpsc::channel(1);
        let new_id = SessionId::generate();

        log::info!("Initializing new session: {}", new_id);

        {
            self.state
                .write()
                .await
                .starting_session
                .entry(new_id)
                .or_insert(sender);
        }

        // TODO: Add timeout for session initialization.
        // TODO: We should spawn in different thread, but it's impossible since
        //       `init_session` starts actor and tokio panics.
        tokio::task::spawn_local(async move {
            if let Err(e) = self.clone().init_session(receiver, with, new_id).await {
                log::warn!("Error initializing session [{}], {}", new_id, e);

                // Establishing session failed.
                self.error_response(new_id.vec(), &with, e).await;
                self.cleanup_initialization(&new_id).await;
            }
        });
        Ok(())
    }

    async fn establish_session(
        self,
        id: SessionId,
        _from: SocketAddr,
        packet: Option<proto::packet::Kind>,
    ) -> ServerResult<()> {
        let mut sender = {
            match { self.state.read().await.starting_session.get(&id).cloned() } {
                Some(sender) => sender.clone(),
                None => return Err(Unauthorized::SessionNotFound(id).into()),
            }
        };

        let request = match packet {
            Some(proto::packet::Kind::Request(request)) => request,
            _ => return Err(BadRequest::InvalidPacket(id, "Request".to_string()).into()),
        };

        Ok(sender
            .send(request)
            .await
            .map_err(|_| InternalError::Send)?)
    }

    async fn init_session(
        self,
        mut rc: mpsc::Receiver<proto::Request>,
        with: SocketAddr,
        session_id: SessionId,
    ) -> ServerResult<()> {
        let challenge = PacketKind::Packet(proto::Packet {
            session_id: session_id.vec(),
            kind: Some(proto::packet::Kind::Control(proto::Control {
                kind: Some(proto::control::Kind::Challenge(Challenge {
                    version: "0.0.1".to_string(),
                    caps: 0,
                    kind: 0,
                    difficulty: 0,
                    challenge: rand::thread_rng().gen::<[u8; CHALLENGE_SIZE]>().to_vec(),
                })),
            })),
        });

        self.send_to(challenge, &with)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!("Challenge sent to: {}", with);

        let node = match rc.next().await {
            Some(proto::Request {
                kind: Some(proto::request::Kind::Session(session)),
            }) => {
                log::info!("Got challenge from node: {}", with);

                // TODO: Validate challenge.

                if session.node_id.len() != 20 {
                    return Err(BadRequest::InvalidNodeId.into());
                }

                let node_id = NodeId::from(&session.node_id[..]);
                let info = NodeInfo {
                    node_id,
                    public_key: session.public_key,
                    slot: u32::max_value(),
                    endpoints: vec![],
                };

                let node = NodeSession {
                    info,
                    session: session_id,
                    last_seen: Utc::now(),
                };

                self.send_to(PacketKind::session_response(session_id), &with)
                    .await
                    .map_err(|_| InternalError::Send)?;

                log::info!(
                    "Session: {}. Got valid challenge from node: {}",
                    session_id,
                    node_id
                );

                node
            }
            _ => return Err(BadRequest::InvalidPacket(session_id, "Session".to_string()).into()),
        };

        match rc.next().await {
            Some(proto::Request {
                kind: Some(proto::request::Kind::Register(registration)),
            }) => {
                log::info!("Got register from node: {}", with);

                let node_id = node.info.node_id;
                let node = self
                    .register_endpoints(session_id, with, registration, node)
                    .await?;

                self.cleanup_initialization(&session_id).await;

                {
                    let mut server = self.state.write().await;
                    server.nodes.register(node);
                }

                log::info!("Session: {} established for node: {}", session_id, node_id);
            }
            _ => return Err(BadRequest::InvalidPacket(session_id, "Register".to_string()).into()),
        };
        Ok(())
    }

    async fn cleanup_initialization(&self, session_id: &SessionId) {
        self.state.write().await.starting_session.remove(session_id);
    }

    async fn send_to(&self, packet: PacketKind, target: &SocketAddr) -> anyhow::Result<()> {
        Ok(self.inner.socket.clone().send((packet, *target)).await?)
    }

    pub async fn bind_udp(addr: url::Url) -> anyhow::Result<Server> {
        let (input, output, addr) = udp_bind(addr.clone()).await?;
        let url = Url::parse(&format!("udp://{}:{}", addr.ip(), addr.port()))?;

        Server::bind(url, input, output)
    }

    pub fn bind(addr: url::Url, input: InStream, output: OutStream) -> anyhow::Result<Server> {
        let inner = Arc::new(ServerImpl {
            socket: output,
            url: addr,
        });

        let state = Arc::new(RwLock::new(ServerState {
            nodes: NodesState::new(),
            starting_session: Default::default(),
            recv_socket: Some(input),
        }));

        Ok(Server { state, inner })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let server = self.clone();
        let mut input = {
            self.state
                .write()
                .await
                .recv_socket
                .take()
                .ok_or_else(|| anyhow!("Server already running."))?
        };

        while let Some((packet, addr)) = input.next().await {
            let id = PacketKind::session(&packet);

            let error = match server.dispatch(addr, packet).await {
                Ok(_) => continue,
                Err(e) => e,
            };

            log::error!("Packet dispatch failed. {}", error);

            server.error_response(id, &addr, error).await;
        }

        log::info!("Server stopped.");
        Ok(())
    }

    async fn error_response(&self, id: Vec<u8>, addr: &SocketAddr, error: Error) {
        let response = match error {
            Error::Undefined(_) => PacketKind::error(id, StatusCode::Undefined),
            Error::BadRequest(_) => PacketKind::error(id, StatusCode::BadRequest),
            Error::Unauthorized(_) => PacketKind::error(id, StatusCode::Unauthorized),
            Error::NotFound(_) => PacketKind::error(id, StatusCode::NotFound),
            Error::Timeout(_) => PacketKind::error(id, StatusCode::Timeout),
            Error::Conflict(_) => PacketKind::error(id, StatusCode::Conflict),
            Error::PayloadTooLarge(_) => PacketKind::error(id, StatusCode::PayloadTooLarge),
            Error::TooManyRequests(_) => PacketKind::error(id, StatusCode::TooManyRequests),
            Error::Internal(_) => PacketKind::error(id, StatusCode::ServerError),
            Error::GatewayTimeout(_) => PacketKind::error(id, StatusCode::GatewayTimeout),
        };

        self.send_to(response, addr)
            .await
            .map_err(|e| log::error!("Failed to send error response. {}.", e))
            .ok();
    }
}

pub fn parse_udp_url(url: Url) -> anyhow::Result<String> {
    let host = url.host_str().context("Needs host for NET URL")?;
    let port = url.port().unwrap_or(DEFAULT_NET_PORT);

    Ok(format!("{}:{}", host, port))
}
