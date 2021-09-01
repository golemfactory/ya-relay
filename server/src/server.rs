use anyhow::{anyhow, Context};
use bytes::BytesMut;
use chrono::Utc;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use rand::Rng;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tokio_util::codec::{Decoder, Encoder};
use url::Url;

use crate::challenge::{Challenge, DefaultChallenge};
use crate::error::{
    BadRequest, Error, InternalError, NotFound, ServerResult, Timeout, Unauthorized,
};
use crate::session::{Endpoint, NodeInfo, NodeSession, SessionId};
use crate::state::NodesState;
use crate::udp_stream::{udp_bind, InStream, OutStream};

use ya_client_model::NodeId;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{PacketKind, MAX_PACKET_SIZE};
use ya_relay_proto::proto;
use ya_relay_proto::proto::request::Kind;
use ya_relay_proto::proto::{RequestId, StatusCode};

pub const DEFAULT_NET_PORT: u16 = 7464;
pub const CHALLENGE_SIZE: usize = 16;
pub const CHALLENGE_DIFFICULTY: u64 = 16;

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
                            request_id,
                            kind: Some(proto::request::Kind::Session(_)),
                        })) => {
                            return self.clone().new_session(request_id, from).await;
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
                    Some(proto::packet::Kind::Request(proto::Request {
                        request_id,
                        kind: Some(kind),
                    })) => match kind {
                        Kind::Session(_) => {}
                        Kind::Register(_) => {}
                        Kind::Node(params) => {
                            self.node_request(request_id, id, from, params).await?
                        }
                        Kind::Neighbours(_) => {}
                        Kind::ReverseConnection(_) => {}
                        Kind::Ping(_) => self.ping_request(request_id, id, from).await?,
                    },
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

            PacketKind::Forward(forward) => self.forward(forward, from).await?,
            PacketKind::ForwardCtd(_) => {
                log::info!("ForwardCtd packet from: {}", from)
            }
        };

        Ok(())
    }

    async fn forward(&self, mut packet: proto::Forward, _from: SocketAddr) -> ServerResult<()> {
        let session_id = SessionId::from(packet.session_id);
        let slot = packet.slot;

        let (src_node, dest_node) = {
            let server = self.state.read().await;

            // Authorization: Sending Node must have established session.
            // TODO: This operation has log(n) complexity. It wastes optimization in `get_by_slot`
            //       which has O(1) complexity.
            let src_node = server
                .nodes
                .get_by_session(session_id)
                .ok_or(Unauthorized::SessionNotFound(session_id.clone()))?;

            let dest_node = server
                .nodes
                .get_by_slot(slot)
                .ok_or(NotFound::NodeBySlot(slot))?;
            (src_node, dest_node)
        };

        if dest_node.info.endpoints.len() > 0 {
            // TODO: How to chose best endpoint?
            let endpoint = dest_node.info.endpoints[0].clone();

            // Replace destination slot and session id with sender slot and session_id.
            // Note: We can unwrap since SessionId type has the same array length.
            packet.slot = src_node.info.slot;
            packet.session_id = src_node.session.to_vec().as_slice().try_into().unwrap();

            log::debug!("Sending forward packet to {}", endpoint.address);

            self.send_to(PacketKind::Forward(packet), &endpoint.address)
                .await
                .map_err(|_| InternalError::Send)?;
        } else {
            log::info!(
                "Can't forward packet for session [{}]. Node [{}] has no public address.",
                session_id,
                dest_node.info.node_id
            );
        }

        // TODO: We should update `last_seen` timestamp of `src_node`.
        Ok(())
    }

    async fn public_endpoints(
        &self,
        session_id: SessionId,
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
        let ping = proto::Packet::request(session_id.to_vec(), proto::request::Ping {}).into();

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
            _ => return Err(BadRequest::InvalidPacket(session_id, "Pong".to_string()).into()),
        };

        Ok(vec![Endpoint {
            protocol: proto::Protocol::Udp,
            address: addr.clone(),
        }])
    }

    async fn register_endpoints(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
        _params: proto::request::Register,
        mut session: NodeSession,
    ) -> ServerResult<NodeSession> {
        // TODO: Note that we ignore endpoints sent by Node and only try
        //       to verify address, from which we received messages.

        session
            .info
            .endpoints
            .extend(self.public_endpoints(session_id, &from).await?.into_iter());

        let endpoints = session
            .info
            .endpoints
            .iter()
            .cloned()
            .map(proto::Endpoint::from)
            .collect();
        let response = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            proto::response::Register { endpoints },
        );

        self.send_to(response, &from)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!("Responding to Register from: {}", from);
        Ok(session)
    }

    async fn ping_request(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
    ) -> ServerResult<()> {
        {
            let mut server = self.state.write().await;
            server.nodes.update_seen(session_id)?;
        }

        self.send_to(
            proto::Packet::response(
                request_id,
                session_id.to_vec(),
                proto::StatusCode::Ok,
                proto::response::Pong {},
            ),
            &from,
        )
        .await
        .map_err(|_| InternalError::Send)?;

        log::info!("Responding to ping from: {}", from);

        Ok(())
    }

    async fn node_request(
        &self,
        request_id: RequestId,
        session_id: SessionId,
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

        let public_key = if params.public_key {
            node_info.info.public_key
        } else {
            vec![]
        };
        let node = proto::response::Node {
            node_id: node_info.info.node_id.into_array().to_vec(),
            public_key,
            endpoints: node_info
                .info
                .endpoints
                .into_iter()
                .map(proto::Endpoint::from)
                .collect(),
            seen_ts: node_info.last_seen.timestamp() as u32,
            slot: node_info.info.slot,
            random: false,
        };

        self.send_to(
            proto::Packet::response(request_id, session_id.to_vec(), proto::StatusCode::Ok, node),
            &from,
        )
        .await
        .map_err(|_| InternalError::Send)?;

        log::info!("Node [{}] info sent to: {}", node_id, from);

        Ok(())
    }

    async fn new_session(self, request_id: RequestId, with: SocketAddr) -> ServerResult<()> {
        let (sender, receiver) = mpsc::channel(1);
        let session_id = SessionId::generate();

        log::info!("Initializing new session: {}", session_id);

        {
            self.state
                .write()
                .await
                .starting_session
                .entry(session_id)
                .or_insert(sender);
        }

        // TODO: Add timeout for session initialization.
        // TODO: We should spawn in different thread, but it's impossible since
        //       `init_session` starts actor and tokio panics.
        tokio::task::spawn_local(async move {
            if let Err(e) = self
                .clone()
                .init_session(with, request_id, session_id, receiver)
                .await
            {
                log::warn!("Error initializing session [{}], {}", session_id, e);

                // Establishing session failed.
                self.error_response(request_id, session_id.to_vec(), &with, e)
                    .await;
                self.cleanup_initialization(&session_id).await;
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
        with: SocketAddr,
        request_id: RequestId,
        session_id: SessionId,
        mut rc: mpsc::Receiver<proto::Request>,
    ) -> ServerResult<()> {
        let raw_challenge = rand::thread_rng().gen::<[u8; CHALLENGE_SIZE]>();
        let challenge = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            proto::response::Challenge {
                version: "0.0.1".to_string(),
                caps: 0,
                kind: 10,
                difficulty: CHALLENGE_DIFFICULTY as u64,
                challenge: raw_challenge.to_vec(),
            },
        );

        self.send_to(challenge, &with)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!("Challenge sent to: {}", with);

        let node = match rc.next().await {
            Some(proto::Request {
                request_id,
                kind: Some(proto::request::Kind::Session(session)),
            }) => {
                log::info!("Got challenge from node: {}", with);

                // Validate the challenge
                if !DefaultChallenge::with(session.public_key.as_slice())
                    .validate(
                        &raw_challenge,
                        CHALLENGE_DIFFICULTY,
                        &session.challenge_resp,
                    )
                    .map_err(|e| BadRequest::InvalidChallenge(e.to_string()))?
                {
                    return Err(Unauthorized::InvalidChallenge.into());
                }

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

                self.send_to(
                    proto::Packet::response(
                        request_id,
                        session_id.to_vec(),
                        proto::StatusCode::Ok,
                        proto::response::Session {},
                    ),
                    &with,
                )
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
                request_id,
                kind: Some(proto::request::Kind::Register(registration)),
            }) => {
                log::info!("Got register from node: {}", with);

                let node_id = node.info.node_id;
                let node = self
                    .register_endpoints(request_id, session_id, with, registration, node)
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

    async fn send_to(
        &self,
        packet: impl Into<PacketKind>,
        target: &SocketAddr,
    ) -> anyhow::Result<()> {
        Ok(self
            .inner
            .socket
            .clone()
            .send((packet.into(), *target))
            .await?)
    }

    pub async fn bind_udp(addr: url::Url) -> anyhow::Result<Server> {
        let (input, output, addr) = udp_bind(&addr).await?;
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
            let request_id = PacketKind::request_id(&packet);
            let session_id = PacketKind::session_id(&packet);
            let error = match server.dispatch(addr, packet).await {
                Ok(_) => continue,
                Err(e) => e,
            };

            log::error!("Packet dispatch failed. {}", error);
            if let Some(request_id) = request_id {
                server
                    .error_response(request_id, session_id, &addr, error)
                    .await;
            }
        }

        log::info!("Server stopped.");
        Ok(())
    }

    async fn error_response(&self, req_id: u64, id: Vec<u8>, addr: &SocketAddr, error: Error) {
        let status_code = match error {
            Error::Undefined(_) => StatusCode::Undefined,
            Error::BadRequest(_) => StatusCode::BadRequest,
            Error::Unauthorized(_) => StatusCode::Unauthorized,
            Error::NotFound(_) => StatusCode::NotFound,
            Error::Timeout(_) => StatusCode::Timeout,
            Error::Conflict(_) => StatusCode::Conflict,
            Error::PayloadTooLarge(_) => StatusCode::PayloadTooLarge,
            Error::TooManyRequests(_) => StatusCode::TooManyRequests,
            Error::Internal(_) => StatusCode::ServerError,
            Error::GatewayTimeout(_) => StatusCode::GatewayTimeout,
        };

        self.send_to(proto::Packet::error(req_id, id, status_code), addr)
            .await
            .map_err(|e| log::error!("Failed to send error response. {}.", e))
            .ok();
    }
}

pub fn dispatch_response(packet: PacketKind) -> Result<proto::response::Kind, proto::StatusCode> {
    match packet {
        PacketKind::Packet(proto::Packet {
            kind: Some(proto::packet::Kind::Response(proto::Response { kind, code, .. })),
            ..
        }) => match kind {
            None => {
                Err(proto::StatusCode::from_i32(code as i32).ok_or(proto::StatusCode::Undefined)?)
            }
            Some(response) => {
                match proto::StatusCode::from_i32(code as i32) {
                    Some(proto::StatusCode::Ok) => Ok(response),
                    _ => Err(proto::StatusCode::from_i32(code as i32)
                        .ok_or(proto::StatusCode::Undefined)?),
                }
            }
        },
        _ => Err(proto::StatusCode::Undefined),
    }
}

pub fn parse_udp_url(url: &Url) -> anyhow::Result<String> {
    let host = url.host_str().context("Needs host for NET URL")?;
    let port = url.port().unwrap_or(DEFAULT_NET_PORT);

    Ok(format!("{}:{}", host, port))
}
