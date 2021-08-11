use anyhow::{anyhow, bail};
use bytes::BytesMut;
use ethsign::SecretKey;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio_util::codec::{Decoder, Encoder};
use url::Url;

use crate::packets::{dispatch_response, PacketsCreator};
use crate::server::Server;
use crate::testing::dispatcher::Dispatcher;
use crate::testing::key;
use crate::udp_stream::udp_bind;
use crate::{parse_udp_url, SessionId};

use ya_client_model::NodeId;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::*;
use ya_relay_proto::proto;

#[derive(Clone)]
pub struct Client {
    pub inner: Arc<RwLock<ClientImpl>>,
}

pub struct ClientBuilder {
    secret: Option<SecretKey>,
    url: Url,

    auto_connect: bool,
}

pub struct ClientImpl {
    pub net_address: SocketAddr,
    pub dispatcher: Dispatcher,
    secret: SecretKey,

    pub session: Option<SessionId>,
}

impl ClientBuilder {
    pub fn from_server(server: &Server) -> ClientBuilder {
        let url = { server.inner.url.clone() };
        ClientBuilder::from_url(url)
    }

    pub fn from_url(url: Url) -> ClientBuilder {
        ClientBuilder {
            secret: None,
            url,
            auto_connect: false,
        }
    }

    pub fn with_secret(mut self, secret: SecretKey) -> ClientBuilder {
        self.secret = Some(secret);
        self
    }

    pub fn connect(mut self) -> ClientBuilder {
        self.auto_connect = true;
        self
    }

    pub async fn build(&self) -> anyhow::Result<Client> {
        let client = Client::new(self).await?;

        if self.auto_connect {
            client.init_session().await?;
            client.register_endpoints(vec![]).await?;
        }

        Ok(client)
    }
}

impl Client {
    pub async fn new(builder: &ClientBuilder) -> anyhow::Result<Client> {
        let (input, output, url) = udp_bind(Url::parse("udp://0.0.0.0:0")?)?;

        let url = builder.url.clone();
        let secret = builder.secret.clone().unwrap_or_else(key::generate);

        Ok(Client {
            inner: Arc::new(RwLock::new(ClientImpl {
                net_address: parse_udp_url(url)?.parse()?,
                dispatcher: Dispatcher::,
                secret,
                session: None,
            })),
        })
    }

    pub async fn id(&self) -> NodeId {
        NodeId::from(*self.inner.read().await.secret.public().address())
    }

    pub async fn session_id(&self) -> anyhow::Result<SessionId> {
        self.inner
            .read()
            .await
            .session
            .ok_or(anyhow!("Session not initialized"))
    }

    pub async fn register_endpoints(
        &self,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        let session_id = self.session_id().await?;
        self.register_endpoints_with_session(session_id, endpoints)
            .await
    }

    pub async fn register_endpoints_with_session(
        &self,
        session_id: SessionId,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        let sent = self
            .send_packet(register_endpoints_packet(session_id, endpoints))
            .await?;

        log::info!("Register endpoints packet sent ({} bytes).", sent);

        // We expect to get ping from Server, which has to check if we have public IP.
        // Note, that server used different port, than we used before.
        let (packet, from) = self
            .receive_packet_from()
            .await
            .map_err(|e| anyhow!("Didn't receive ping. Error: {}", e))?;

        match packet {
            PacketKind::Packet(proto::Packet {
                session_id: _,
                kind:
                    Some(proto::packet::Kind::Request(proto::Request {
                        kind: Some(proto::request::Kind::Ping(proto::request::Ping {})),
                    })),
            }) => (),
            _ => bail!("Invalid packet kind. `Ping` expected."),
        };

        log::info!("Ping received. Sending `Pong`.");

        self.send_packet_to(PacketKind::pong_response(session_id), from)
            .await?;

        log::info!("Waiting for Register response.");

        let packet = self
            .receive_packet()
            .await
            .map_err(|e| anyhow!("Didn't receive register response. Error: {}", e))?;

        log::info!("Decoded packet received from server. {:?}", packet);

        let endpoints = match dispatch_response(packet) {
            Ok(proto::response::Kind::Register(proto::response::Register { endpoints })) => {
                endpoints
            }
            Ok(_) => bail!("Invalid packet kind. `Register` response expected."),
            Err(code) => bail!("Error response from server. Code: {}", code as i32),
        };

        log::info!("Registration finished.");
        Ok(endpoints)
    }

    pub async fn init_session(&self) -> anyhow::Result<SessionId> {
        let sent = self.send_packet(init_packet()).await?;

        log::info!("Init session packet sent ({} bytes).", sent);

        let packet = self
            .receive_packet()
            .await
            .map_err(|e| anyhow!("Didn't receive challenge. Error: {}", e))?;

        log::info!("Decoded packet received from server. {:?}", packet);

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

        let session = match dispatch_response(packet) {
            Ok(proto::response::Kind::Session(_)) => session,
            Err(code) => bail!("Session [{}] response code: {}", session, code as i32),
            _ => bail!("Invalid packet kind. Expected Session response."),
        };

        {
            self.inner.write().await.session = Some(session);
        }

        Ok(session)
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let session_id = self.session_id().await?;
        self.ping_with_session(session_id).await
    }

    pub async fn ping_with_session(&self, session_id: SessionId) -> anyhow::Result<()> {
        self.send_packet(ping_packet(session_id)).await?;

        log::info!("Ping sent to server");

        let packet = self
            .receive_packet()
            .await
            .map_err(|e| anyhow!("Didn't receive Pong response. Error: {}", e))?;

        match dispatch_response(packet) {
            Ok(proto::response::Kind::Pong(_)) => log::info!("Got ping from server."),
            Err(e) => bail!("Error response for Ping. Code {:?}", e),
            _ => bail!("Invalid Response packet. Expected Pong"),
        }

        Ok(())
    }

    pub async fn find_node(&self, node_id: NodeId) -> anyhow::Result<proto::response::Node> {
        let session_id = self.session_id().await?;
        self.find_node_with_session(session_id, node_id).await
    }

    pub async fn find_node_with_session(
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

        match dispatch_response(packet) {
            Ok(proto::response::Kind::Node(node_info)) => {
                log::info!("Got info for node: [{}]", node_id);
                Ok(node_info)
            }
            Ok(_) => bail!("Invalid packet kind. `Node` response expected."),
            Err(code) => bail!("Error response from server. Code: {}", code as i32),
        }
    }

    async fn send_packet_to(&self, packet: PacketKind, addr: SocketAddr) -> anyhow::Result<usize> {
        let mut codec = Codec::default();
        let mut out_buf = BytesMut::new();

        codec.encode(packet, &mut out_buf)?;

        {
            let mut client = self.inner.write().await;
            Ok(client.socket.send_to(&out_buf, addr).await?)
        }
    }

    async fn send_packet(&self, packet: PacketKind) -> anyhow::Result<usize> {
        let addr = { self.inner.write().await.net_address };
        self.send_packet_to(packet, addr).await
    }

    async fn receive_packet_from(&self) -> anyhow::Result<(PacketKind, SocketAddr)> {
        let mut codec = Codec::default();
        let mut in_buf = BytesMut::new();

        in_buf.resize(MAX_PACKET_SIZE as usize, 0);

        let (size, addr) = {
            let client = &mut self.inner.write().await;
            client.socket.recv_from(&mut in_buf).await?
        };

        in_buf.truncate(size);

        let packet = codec
            .decode(&mut in_buf)?
            .ok_or_else(|| anyhow!("Failed to decode packet."))?;

        Ok((packet, addr))
    }

    async fn receive_packet(&self) -> anyhow::Result<PacketKind> {
        Ok(self.receive_packet_from().await?.0)
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

fn register_endpoints_packet(session_id: SessionId, endpoints: Vec<proto::Endpoint>) -> PacketKind {
    PacketKind::Packet(proto::Packet {
        session_id: session_id.vec(),
        kind: Some(proto::packet::Kind::Request(proto::Request {
            kind: Some(proto::request::Kind::Register(proto::request::Register {
                endpoints,
            })),
        })),
    })
}
