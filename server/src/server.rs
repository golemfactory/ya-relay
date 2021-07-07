use actix::prelude::*;

use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;
use tokio::sync::RwLock;
use url::Url;

use crate::session::{NodeSession, RequestPacket, SessionId};

use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto::*;

pub const DEFAULT_NET_PORT: u16 = 7464;

#[derive(Clone)]
pub struct Server {
    inner: Arc<RwLock<ServerImpl>>,
}

pub struct ServerImpl {
    sessions: HashMap<SessionId, Addr<NodeSession>>,
}

impl Server {
    pub fn new() -> anyhow::Result<Server> {
        Ok(Server {
            inner: Arc::new(RwLock::new(ServerImpl {
                sessions: Default::default(),
            })),
        })
    }

    pub async fn dispatch(&self, packet: PacketKind) -> anyhow::Result<()> {
        match packet {
            PacketKind::Packet(Packet { kind }) => match kind {
                Some(packet::Kind::Request(request)) => {
                    log::info!("Request packet");

                    let id = SessionId::try_from(request.session_id.clone())?;
                    match self.inner.read().await.sessions.get(&id).cloned() {
                        None => self.clone().init_session().await?,
                        Some(node) => node.send(RequestPacket(request)).await??,
                    }
                }
                Some(packet::Kind::Response(_)) => {
                    log::info!("Response packet");
                }
                Some(packet::Kind::Control(_)) => {
                    log::info!("Control packet");
                }
                _ => log::info!("Packet kind: None"),
            },
            PacketKind::Forward(_) => {
                log::info!("Forward packet")
            }
            PacketKind::ForwardCtd(_) => {
                log::info!("ForwardCtd packet")
            }
        };

        Ok(())
    }

    async fn init_session(self) -> anyhow::Result<()> {
        log::info!("Initializing new session");
        Ok(())
    }
}

pub fn parse_udp_url(url: Url) -> String {
    let host = url.host_str().expect("Needs host for NET URL");
    let port = url.port().unwrap_or(DEFAULT_NET_PORT);

    format!("{}:{}", host, port)
}
