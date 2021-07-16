use anyhow::bail;
use bytes::BytesMut;
use rand::Rng;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::udp::SendHalf;
use tokio::sync::{mpsc, RwLock};
use tokio_util::codec::Encoder;
use url::Url;

use crate::session::{NodeInfo, NodeSession, SessionId};

use ya_client_model::NodeId;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::control::Challenge;
use ya_relay_proto::proto::request::Kind;
use ya_relay_proto::proto::response::Node;

pub const DEFAULT_NET_PORT: u16 = 7464;
pub const CHALLENGE_SIZE: usize = 16;

#[derive(Clone)]
pub struct Server {
    inner: Arc<RwLock<ServerImpl>>,
}

pub struct ServerImpl {
    sessions: HashMap<SessionId, NodeId>,
    nodes: HashMap<NodeId, NodeSession>,
    init_session: HashMap<SessionId, mpsc::Sender<proto::Request>>,

    /// TODO: Inefficient. We need to acquire lock to send data. But in this version
    ///       of tokio, sockets need `mut self` and it is not possible to upgrade,
    ///       without doing this in whole yagna.
    socket: SendHalf,
}

impl Server {
    pub fn new(socket: SendHalf) -> anyhow::Result<Server> {
        Ok(Server {
            inner: Arc::new(RwLock::new(ServerImpl {
                sessions: Default::default(),
                nodes: Default::default(),
                init_session: Default::default(),
                socket,
            })),
        })
    }

    pub async fn dispatch(&self, from: SocketAddr, packet: PacketKind) -> anyhow::Result<()> {
        match packet {
            PacketKind::Packet(proto::Packet { kind, session_id }) => {
                if session_id.is_empty() {
                    return Ok(self.clone().new_session(from).await?);
                }

                let id = SessionId::try_from(session_id)?;
                let node = match self.inner.read().await.sessions.get(&id).cloned() {
                    None => return Ok(self.clone().establish_session(id, kind).await?),
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
                            Kind::Ping(_) => {}
                        }
                    }
                    Some(proto::packet::Kind::Response(_)) => {
                        log::warn!("Server shouldn't get Response packet. Sender: {}", from);
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

    async fn node_request(
        &self,
        id: SessionId,
        from: SocketAddr,
        params: proto::request::Node,
    ) -> anyhow::Result<()> {
        let node_id = NodeId::from(&params.node_id[..]);
        let (node_info, endpoints) = {
            match self.inner.read().await.nodes.get(&node_id) {
                None => bail!("Node [{}] not registered.", node_id),
                Some(session) => (session.info.clone(), session.endpoints.clone()),
            }
        };

        let public_key = match params.public_key {
            true => node_info.public_key,
            false => vec![],
        };

        let response = PacketKind::Packet(proto::Packet {
            session_id: id.vec(),
            kind: Some(proto::packet::Kind::Response(proto::Response {
                code: 0,
                kind: Some(proto::response::Kind::Node(Node {
                    node_id: node_id.into_array().to_vec(),
                    public_key,
                    endpoints,
                    seen_ts: 0,
                    slot: 0,
                    random: false,
                })),
            })),
        });

        self.send_to(response, &from).await?;
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
                .init_session
                .entry(new_id)
                .or_insert(sender);
        }

        // TODO: Add timeout for session initialization.
        // TODO: We should spawn in different thread, but it's impossible since
        //       `init_session` starts actor and tokio panics.
        tokio::task::spawn_local(async move {
            if let Err(e) = self
                .clone()
                .init_session(receiver, with, new_id.clone())
                .await
            {
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
            match self.inner.read().await.init_session.get(&id) {
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
                    challenge: rand::thread_rng()
                        .gen::<[u8; CHALLENGE_SIZE]>()
                        .iter()
                        .cloned()
                        .collect(),
                })),
            })),
        });

        self.send_to(challenge, &with).await?;

        log::info!("Challenge sent to: {}", with);

        match rc.recv().await {
            Some(proto::Request { kind }) => match kind {
                Some(proto::request::Kind::Session(session)) => {
                    log::info!("Got challenge from node: {}", with);

                    // TODO: Validate challenge.

                    let node_id = NodeId::from(&session.node_id[..]);
                    let info = NodeInfo {
                        session: session_id.clone(),
                        node_id: node_id.clone(),
                        public_key: session.public_key,
                        address: with.clone(),
                    };

                    let node = NodeSession::new(info);
                    self.cleanup_initialization(&session_id).await;

                    {
                        let mut server = self.inner.write().await;

                        server.sessions.insert(session_id.clone(), node_id.clone());
                        server.nodes.insert(node_id.clone(), node);
                    }

                    log::info!("Session: {} established for node: {}", session_id, node_id);
                }
                _ => bail!("Invalid Request"),
            },
            _ => bail!("Invalid Request"),
        };
        Ok(())
    }

    async fn cleanup_initialization(&self, session_id: &SessionId) {
        self.inner.write().await.init_session.remove(session_id);
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
            .send_to(&buf, &target)
            .await?)
    }
}

pub fn parse_udp_url(url: Url) -> String {
    let host = url.host_str().expect("Needs host for NET URL");
    let port = url.port().unwrap_or(DEFAULT_NET_PORT);

    format!("{}:{}", host, port)
}
