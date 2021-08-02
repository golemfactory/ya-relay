use anyhow::{anyhow, bail};
use bytes::BytesMut;
use ethsign::SecretKey;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio_util::codec::{Decoder, Encoder};

use crate::server::Server;
use crate::{parse_udp_url, SessionId};

use std::convert::TryFrom;
use url::Url;
use ya_client_model::NodeId;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::*;
use ya_relay_proto::proto;
use ya_relay_proto::proto::packet::Kind;

use crate::testing::key;

#[derive(Clone)]
pub struct Client {
    inner: Arc<RwLock<ClientImpl>>,
}

pub struct ClientImpl {
    net_address: SocketAddr,
    socket: UdpSocket,
    secret: SecretKey,
}

impl Client {
    pub async fn connect(server: &Server, secret: Option<SecretKey>) -> anyhow::Result<Client> {
        let addr = { server.inner.read().await.url.clone() };
        Ok(Client::bind(addr, secret).await?)
    }

    pub async fn bind(addr: Url, secret: Option<SecretKey>) -> anyhow::Result<Client> {
        let local_addr: SocketAddr = "0.0.0.0:0".parse()?;
        let socket = UdpSocket::bind(local_addr).await?;
        let secret = secret.unwrap_or(key::generate());

        Ok(Client {
            inner: Arc::new(RwLock::new(ClientImpl {
                net_address: parse_udp_url(addr)?.parse()?,
                socket,
                secret,
            })),
        })
    }

    pub async fn init_session(&self) -> anyhow::Result<SessionId> {
        let sent = self.send_packet(init_packet()).await?;

        log::info!("Init session packet sent ({} bytes).", sent);

        let packet = self
            .receive_packet()
            .await
            .map_err(|e| anyhow!("Didn't receive challenge. Error: {}", e))?;

        log::info!("Decoded packet received from server.");

        let session = SessionId::try_from(match packet {
            PacketKind::Packet(proto::Packet { session_id, kind }) => match kind {
                Some(proto::packet::Kind::Control(proto::Control { kind })) => match kind {
                    Some(proto::control::Kind::Challenge(proto::control::Challenge { .. })) => {
                        session_id
                    }
                    _ => bail!("Expected Challenge packet."),
                },
                _ => bail!("Invalid packet kind."),
            },
            _ => bail!("Expected Control packet with challenge."),
        })?;

        log::info!(
            "Decoded packet is correct Challenge. Session id: {}",
            session
        );

        let node_id = NodeId::from(*self.inner.read().await.secret.public().address());
        let sent = self.send_packet(session_packet(session, node_id)).await?;

        log::info!("Challenge response sent ({} bytes).", sent);

        let packet = self
            .receive_packet()
            .await
            .map_err(|e| anyhow!("Didn't receive session response. Error: {}", e))?;

        match packet {
            PacketKind::Packet(proto::Packet {
                session_id,
                kind:
                    Some(proto::packet::Kind::Response(proto::Response {
                        code,
                        kind: Some(proto::response::Kind::Session(proto::response::Session {})),
                    })),
            }) => match proto::StatusCode::from_i32(code) {
                Some(proto::StatusCode::Ok) => Ok(session),
                _ => Err(anyhow!(
                    "Session [{}] response code: {}",
                    SessionId::try_from(session_id)?,
                    code as i32
                )),
            },
            _ => bail!("Invalid packet kind."),
        }
    }

    pub async fn ping(&self, session_id: SessionId) -> anyhow::Result<()> {
        self.send_packet(ping_packet(session_id)).await?;

        log::info!("Ping sent to server");

        let packet = self
            .receive_packet()
            .await
            .map_err(|e| anyhow!("Didn't receive Pong response. Error: {}", e))?;

        match packet {
            PacketKind::Packet(proto::Packet {
                kind:
                    Some(Kind::Response(proto::Response {
                        kind: Some(proto::response::Kind::Pong(_)),
                        ..
                    })),
                ..
            }) => {
                log::info!("Got ping from server.")
            }
            _ => bail!("Invalid Response."),
        };

        Ok(())
    }

    pub async fn find_node(
        &self,
        session_id: SessionId,
        node_id: NodeId,
    ) -> anyhow::Result<proto::response::Node> {
        self.send_packet(node_packet(session_id, node_id)).await?;

        log::info!("Querying node: [{}] info from server.", node_id);

        let packet = self
            .receive_packet()
            .await
            .map_err(|e| anyhow!("Didn't receive FindNode response. Error: {}", e))?;

        match packet {
            PacketKind::Packet(proto::Packet {
                kind:
                    Some(Kind::Response(proto::Response {
                        kind: Some(proto::response::Kind::Node(node_info)),
                        ..
                    })),
                ..
            }) => {
                log::info!("Got info for node: [{}]", node_id);
                Ok(node_info)
            }
            _ => bail!("Invalid Response."),
        }
    }

    async fn send_packet(&self, packet: PacketKind) -> anyhow::Result<usize> {
        let mut codec = Codec::default();
        let mut out_buf = BytesMut::new();

        codec.encode(packet, &mut out_buf)?;

        {
            let mut client = self.inner.write().await;
            let addr = client.net_address;

            Ok(client.socket.send_to(&out_buf, addr).await?)
        }
    }

    async fn receive_packet(&self) -> anyhow::Result<PacketKind> {
        let mut codec = Codec::default();
        let mut in_buf = BytesMut::new();

        in_buf.resize(MAX_PACKET_SIZE as usize, 0);

        let size = {
            let client = &mut self.inner.write().await;
            client.socket.recv(&mut in_buf).await?
        };

        in_buf.truncate(size);

        Ok(codec
            .decode(&mut in_buf)?
            .ok_or_else(|| anyhow!("Failed to decode packet."))?)
    }
}

fn init_packet() -> PacketKind {
    PacketKind::Packet(proto::Packet {
        session_id: vec![],
        kind: Some(proto::packet::Kind::Request(proto::Request {
            kind: Some(proto::request::Kind::Session(proto::request::Session {
                challenge_resp: vec![],
                node_id: vec![],
                public_key: vec![],
            })),
        })),
    })
}

fn ping_packet(session_id: SessionId) -> PacketKind {
    PacketKind::Packet(proto::Packet {
        session_id: session_id.vec(),
        kind: Some(proto::packet::Kind::Request(proto::Request {
            kind: Some(proto::request::Kind::Ping(proto::request::Ping {})),
        })),
    })
}

fn session_packet(session_id: SessionId, node_id: NodeId) -> PacketKind {
    PacketKind::Packet(proto::Packet {
        session_id: session_id.vec(),
        kind: Some(proto::packet::Kind::Request(proto::Request {
            kind: Some(proto::request::Kind::Session(proto::request::Session {
                challenge_resp: vec![0u8; 2048_usize],
                node_id: node_id.into_array().to_vec(),
                public_key: vec![],
            })),
        })),
    })
}

fn node_packet(session_id: SessionId, node_id: NodeId) -> PacketKind {
    PacketKind::Packet(proto::Packet {
        session_id: session_id.vec(),
        kind: Some(proto::packet::Kind::Request(proto::Request {
            kind: Some(proto::request::Kind::Node(proto::request::Node {
                node_id: node_id.into_array().to_vec(),
                public_key: true,
            })),
        })),
    })
}
