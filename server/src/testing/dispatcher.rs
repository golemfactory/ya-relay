use anyhow::anyhow;
use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::packet::Kind;

use crate::packets::PacketsCreator;
use crate::udp_stream::{InStream, OutStream};

pub struct Dispatcher {
    socket: OutStream,
    recv_socket: Option<InStream>,

    node: broadcast::Sender<Packet<proto::response::Node>>,
    neighbours: broadcast::Sender<Packet<proto::response::Neighbours>>,
    ping: broadcast::Sender<Packet<proto::response::Pong>>,

    forward: broadcast::Sender<Packet<proto::Forward>>,
}

pub struct Packet<T: Sized> {
    packet: T,
    address: SocketAddr,
    code: proto::StatusCode,
}

impl<T> Packet<T>
where
    T: Sized,
{
    pub fn from(packet: T, address: SocketAddr, code: proto::StatusCode) -> Packet<T> {
        Packet {
            packet,
            address,
            code,
        }
    }
}

impl Dispatcher {
    pub fn start_server(mut self) -> anyhow::Result<Arc<Dispatcher>> {
        let mut input = self
            .recv_socket
            .take()
            .ok_or(anyhow!("Client server already initialized."))?;

        let dispatcher = Arc::new(self);
        let server = dispatcher.clone();

        tokio::task::spawn_local(async move {
            while let Some((packet, addr)) = input.next().await {
                if let Err(e) = server.dispatch(addr, packet).await {
                    log::error!("Packet dispatch failed. {}", e);
                };
            }

            log::info!("Client server stopped.");
        });

        Ok(dispatcher)
    }

    pub async fn dispatch(&self, from: SocketAddr, packet: PacketKind) -> anyhow::Result<()> {
        match packet {
            PacketKind::Packet(proto::Packet {
                kind: Some(kind), ..
            }) => match kind {
                Kind::Request(_) => {}
                Kind::Response(proto::Response {
                    code,
                    kind: Some(kind),
                }) => match kind {
                    proto::response::Kind::Session(_) => {}
                    proto::response::Kind::Register(_) => {}
                    proto::response::Kind::Node(packet) => {
                        self.node.send(Packet::from(
                            packet,
                            from,
                            proto::StatusCode::from_i32(code)?,
                        ));
                    }
                    proto::response::Kind::Neighbours(_) => {}
                    proto::response::Kind::Pong(_) => {}
                },
                Kind::Control(_) => {}
                _ => {}
            },
            PacketKind::Forward(forward) => {
                if let Err(e) =
                    self.forward
                        .send(Packet::from(forward, from, proto::StatusCode::Ok))
                {
                    log::error!("Forward packet from: {} dropped", from);
                }
            }
            PacketKind::ForwardCtd(_) => {
                log::warn!("Got not implemented ForwardCtd packet from: {}.", from)
            }
            _ => log::warn!("Got invalid packet from: {}.", from),
        }

        Ok(())
    }
}
