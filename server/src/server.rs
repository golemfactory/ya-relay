use anyhow::{anyhow, bail, Context};
use bytes::BytesMut;
use chrono::Utc;
use rand::Rng;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::udp::{RecvHalf, SendHalf};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock};
use tokio_util::codec::{Decoder, Encoder};
use url::Url;

use crate::error::ServerError;
use crate::packets::PacketsCreator;
use crate::session::{NodeInfo, NodeSession, SessionId};

use ya_client_model::NodeId;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{PacketKind, MAX_PACKET_SIZE};
use ya_relay_proto::proto;
use ya_relay_proto::proto::control::Challenge;
use ya_relay_proto::proto::request::Kind;

pub const DEFAULT_NET_PORT: u16 = 7464;
pub const CHALLENGE_SIZE: usize = 16;

#[derive(Clone)]
pub struct Server {
    pub inner: Arc<RwLock<ServerImpl>>,
}

pub struct ServerImpl {
    pub sessions: HashMap<SessionId, NodeId>,
    pub nodes: HashMap<NodeId, NodeSession>,
    pub starting_session: HashMap<SessionId, mpsc::Sender<proto::Request>>,

    /// TODO: Inefficient. We need to acquire lock to send data. But in this version
    ///       of tokio, sockets need `mut self` and it is not possible to upgrade,
    ///       without doing this in whole yagna.
    socket: SendHalf,
    recv_socket: Option<RecvHalf>,

    pub url: Url,
}

impl Server {
    pub async fn dispatch(&self, from: SocketAddr, packet: PacketKind) -> anyhow::Result<()> {
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
                        _ => bail!("Empty session id."),
                    }
                }

                let id = SessionId::try_from(session_id)?;
                let node = match self.inner.read().await.sessions.get(&id).cloned() {
                    None => return self.clone().establish_session(id, kind).await,
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
                            node,
                        );
                    }
                    Some(proto::packet::Kind::Control(_control)) => {
                        log::info!("Control packet from: {}", from);
                    }
                    _ => log::info!("Packet kind: None from: {}", from),
                }
            }
            PacketKind::Forward(_) => {
                log::info!("Forward packet from: {}", from)
            }
            PacketKind::ForwardCtd(_) => {
                log::info!("ForwardCtd packet from: {}", from)
            }
        };

        Ok(())
    }

    async fn ping_request(&self, id: SessionId, from: SocketAddr) -> anyhow::Result<()> {
        {
            let mut server = self.inner.write().await;
            let node_id = match server.sessions.get(&id) {
                None => return Err(ServerError::SessionNotFound(id).into()),
                Some(node_id) => *node_id,
            };

            let mut node_info = server.nodes.get_mut(&node_id).ok_or_else(|| {
                ServerError::Internal(format!("NodeId for session [{}] not found.", id))
            })?;

            node_info.last_seen = Utc::now();
        }

        let response = PacketKind::pong_response(id);
        self.send_to(response, &from).await?;

        log::info!("Responding to ping from: {}", from);

        Ok(())
    }

    async fn node_request(
        &self,
        id: SessionId,
        from: SocketAddr,
        params: proto::request::Node,
    ) -> anyhow::Result<()> {
        if params.node_id.len() != 20 {
            bail!("Invalid node id.")
        }

        let node_id = NodeId::from(&params.node_id[..]);
        let node_info = {
            match self.inner.read().await.nodes.get(&node_id) {
                None => bail!("Node [{}] not registered.", node_id),
                Some(session) => session.clone(),
            }
        };

        let response = PacketKind::node_response(id, node_info, params.public_key);
        self.send_to(response, &from).await?;

        log::info!("Node [{}] info sent to: {}", node_id, from);

        Ok(())
    }

    async fn new_session(self, with: SocketAddr) -> anyhow::Result<()> {
        let (sender, receiver) = mpsc::channel(1);
        let new_id = SessionId::generate();

        log::info!("Initializing new session: {}", new_id);

        {
            self.inner
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
                self.cleanup_initialization(&new_id).await;
            }
        });
        Ok(())
    }

    async fn establish_session(
        self,
        id: SessionId,
        packet: Option<proto::packet::Kind>,
    ) -> anyhow::Result<()> {
        let mut sender = {
            match self.inner.read().await.starting_session.get(&id) {
                Some(sender) => sender.clone(),
                None => bail!("Session [{}] not initialized.", id),
            }
        };

        let request = match packet {
            Some(proto::packet::Kind::Request(request)) => request,
            _ => bail!("Invalid packet type for session [{}].", id),
        };

        Ok(sender.send(request).await?)
    }

    async fn init_session(
        self,
        mut rc: mpsc::Receiver<proto::Request>,
        with: SocketAddr,
        session_id: SessionId,
    ) -> anyhow::Result<()> {
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

        self.send_to(challenge, &with).await?;

        log::info!("Challenge sent to: {}", with);

        match rc.recv().await {
            Some(proto::Request {
                kind: Some(proto::request::Kind::Session(session)),
            }) => {
                log::info!("Got challenge from node: {}", with);

                // TODO: Validate challenge.

                if session.node_id.len() != 20 {
                    bail!("Invalid node id.")
                }

                let node_id = NodeId::from(&session.node_id[..]);
                let info = NodeInfo {
                    node_id,
                    public_key: session.public_key,
                    endpoints: vec![],
                };

                let node = NodeSession {
                    info,
                    session: session_id,
                    address: with,
                    last_seen: Utc::now(),
                };
                self.cleanup_initialization(&session_id).await;

                {
                    let mut server = self.inner.write().await;

                    server.sessions.insert(session_id, node_id);
                    server.nodes.insert(node_id, node);
                }

                self.send_to(PacketKind::session_response(session_id), &with)
                    .await?;

                log::info!("Session: {} established for node: {}", session_id, node_id);
            }
            _ => bail!("Invalid Request"),
        };
        Ok(())
    }

    async fn cleanup_initialization(&self, session_id: &SessionId) {
        self.inner.write().await.starting_session.remove(session_id);
    }

    async fn send_to(&self, packet: PacketKind, target: &SocketAddr) -> anyhow::Result<usize> {
        let mut codec = Codec::default();
        let mut buf = BytesMut::new();

        codec.encode(packet, &mut buf)?;

        Ok(self
            .inner
            .write()
            .await
            .socket
            .send_to(&buf, target)
            .await?)
    }

    pub async fn bind(addr: url::Url) -> anyhow::Result<Server> {
        let sock = UdpSocket::bind(&parse_udp_url(addr.clone())?).await?;

        log::info!("Server listening on: {}", addr);

        let (input, output) = sock.split();

        Ok(Server {
            inner: Arc::new(RwLock::new(ServerImpl {
                sessions: Default::default(),
                nodes: Default::default(),
                starting_session: Default::default(),
                socket: output,
                recv_socket: Some(input),
                url: addr,
            })),
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let mut input = {
            self.inner
                .write()
                .await
                .recv_socket
                .take()
                .ok_or_else(|| anyhow!("Server already running."))?
        };

        let server = self.clone();
        let mut codec = Codec::default();
        let mut buf = BytesMut::with_capacity(MAX_PACKET_SIZE as usize);

        loop {
            buf.resize(MAX_PACKET_SIZE as usize, 0);

            match input.recv_from(&mut buf).await {
                Ok((size, addr)) => {
                    log::debug!("Received {} bytes from {}", size, addr);

                    buf.truncate(size);
                    match codec.decode(&mut buf) {
                        Ok(Some(packet)) => {
                            server
                                .dispatch(addr, packet)
                                .await
                                .map_err(|e| log::error!("Packet dispatch failed. {}", e))
                                .ok();
                        }
                        Ok(None) => log::warn!("Empty packet."),
                        Err(e) => log::error!("Error: {}", e),
                    }
                }
                Err(e) => {
                    log::error!("Error receiving data from socket. {}", e);
                    return Ok(());
                }
            }
        }
    }
}

pub fn parse_udp_url(url: Url) -> anyhow::Result<String> {
    let host = url.host_str().context("Needs host for NET URL")?;
    let port = url.port().unwrap_or(DEFAULT_NET_PORT);

    Ok(format!("{}:{}", host, port))
}
