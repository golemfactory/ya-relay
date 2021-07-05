use std::sync::Arc;
use tokio::sync::RwLock;
use url::Url;

use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto::*;

pub const DEFAULT_NET_PORT: u16 = 7464;

pub struct Server {}

impl Server {
    pub fn new() -> anyhow::Result<Arc<RwLock<Server>>> {
        Ok(Arc::new(RwLock::new(Server {})))
    }

    pub async fn dispatch(&self, packet: PacketKind) -> anyhow::Result<()> {
        match packet {
            PacketKind::Packet(Packet { kind }) => match kind {
                Some(packet::Kind::Request(_)) => {
                    log::info!("Request packet");
                }
                Some(packet::Kind::Response(_)) => {
                    log::info!("Response packet");
                }
                Some(packet::Kind::Control(_)) => {
                    log::info!("Control packet");
                }
                _ => log::info!("None packet kind"),
            },
            PacketKind::Forward(_) => {
                log::info!("Forward packet")
            }
            PacketKind::ForwardCtd(_) => {
                log::info!("ForwardCtd packet")
            }
        }

        Ok(())
    }
}

pub fn parse_udp_url(url: Url) -> String {
    let host = url.host_str().expect("Needs host for NET URL");
    let port = url.port().unwrap_or(DEFAULT_NET_PORT);

    format!("{}:{}", host, port)
}
