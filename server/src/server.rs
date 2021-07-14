use actix::prelude::*;
use anyhow::bail;

use rand::Rng;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use url::Url;

use crate::session::{ControlPacket, NodeSession, RequestPacket, ResponsePacket, SessionId};

use bytes::BytesMut;
use futures::FutureExt;
use std::net::SocketAddr;
use tokio::net::udp::SendHalf;
use tokio_util::codec::Encoder;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::control::Challenge;
use ya_relay_proto::proto::packet::Kind;
use ya_relay_proto::proto::response::Session;

pub const DEFAULT_NET_PORT: u16 = 7464;
pub const CHALLENGE_SIZE: usize = 16;

#[derive(Clone)]
pub struct Server {
    inner: Arc<RwLock<ServerImpl>>,
}

pub struct ServerImpl {
    sessions: HashMap<SessionId, Addr<NodeSession>>,
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
                    Some(proto::packet::Kind::Request(request)) => {
                        log::info!("Request packet");
                        node.send(RequestPacket(request)).await??
                    }
                    Some(proto::packet::Kind::Response(response)) => {
                        log::info!("Response packet");
                        node.send(ResponsePacket(response)).await??
                    }
                    Some(proto::packet::Kind::Control(control)) => {
                        log::info!("Control packet");
                        node.send(ControlPacket(control)).await??
                    }
                    _ => log::info!("Packet kind: None"),
                }
            }
            PacketKind::Forward(_) => {
                log::info!("Forward packet")
            }
            PacketKind::ForwardCtd(_) => {
                log::info!("ForwardCtd packet")
            }
        };

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
        tokio::spawn(
            self.clone()
                .init_session(receiver, with, new_id.clone())
                .map(move |result| match result {
                    Ok(_) => (),
                    Err(e) => log::warn!("Error initializing session [{}], {}", new_id, e),
                }),
        );
        Ok(())
    }

    async fn establish_session(
        self,
        id: SessionId,
        packet: Option<proto::packet::Kind>,
    ) -> anyhow::Result<()> {
        let request = match packet {
            Some(proto::packet::Kind::Request(request)) => request,
            _ => bail!("Invalid packet type for session [{}].", id),
        };

        let mut sender = {
            match self.inner.read().await.init_session.get(&id) {
                Some(sender) => sender.clone(),
                None => bail!("Session [{}] not initialized.", id),
            }
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
                }
                _ => {}
            },
            _ => bail!("Invalid Request"),
        };
        Ok(())
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
