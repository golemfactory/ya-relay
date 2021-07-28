use anyhow::anyhow;
use bytes::BytesMut;
use futures::channel::mpsc::{channel, Sender};
use futures::prelude::*;
use futures::task::{Context, Poll};
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::net::udp::{RecvHalf, SendHalf};
use tokio::net::UdpSocket;
use tokio_util::codec::{Decoder, Encoder};

use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{PacketKind, MAX_PACKET_SIZE};

use crate::parse_udp_url;

pub type InStream = Pin<Box<dyn Stream<Item = (PacketKind, SocketAddr)>>>;
pub type OutStream = Pin<Box<dyn Sink<(PacketKind, SocketAddr), Error = anyhow::Error>>>;

pub async fn udp_bind(addr: url::Url) -> anyhow::Result<(InStream, OutStream)> {
    let sock = UdpSocket::bind(&parse_udp_url(addr.clone())).await?;

    log::info!("Server listening on: {}", addr);

    let (input, output) = sock.split();

    let stream = Box::pin(udp_stream(input));
    let sink = Box::pin(UdpSink::create(output)?);

    Ok((stream, sink))
}

pub async fn receive_packet(socket: &mut RecvHalf) -> anyhow::Result<(PacketKind, SocketAddr)> {
    let mut codec = Codec::default();
    let mut buf = BytesMut::new();

    buf.resize(MAX_PACKET_SIZE as usize, 0);

    let (size, addr) = socket.recv_from(&mut buf).await?;

    log::debug!("Received {} bytes from {}", size, addr);

    buf.truncate(size);

    let packet = codec
        .decode(&mut buf)?
        .ok_or_else(|| anyhow!("Failed to decode packet."))?;

    Ok((packet, addr))
}

pub fn udp_stream(socket: RecvHalf) -> impl Stream<Item = (PacketKind, SocketAddr)> {
    stream::unfold(socket, |mut socket| async {
        match receive_packet(&mut socket).await {
            Ok(item) => Some((item, socket)),
            Err(e) => {
                log::warn!("Failed to receive packet. {}", e);
                None
            }
        }
    })
}

pub struct UdpSink {
    tx: Sender<(PacketKind, SocketAddr)>,
}

impl UdpSink {
    pub fn create(mut socket: SendHalf) -> anyhow::Result<Self> {
        let (tx, mut rx) = channel(20);

        tokio::task::spawn_local(async move {
            let mut codec = Codec::default();
            let mut buf = BytesMut::with_capacity(MAX_PACKET_SIZE as usize);

            while let Some((packet, target)) = rx.next().await {
                if let Err(e) = codec.encode(packet, &mut buf) {
                    log::warn!("Error encoding packet. {}", e);
                }

                // Error can happen only, if IP version of socket doesn't match,
                // So we can ignore it.
                socket.send_to(&buf, &target).await.ok();
            }
        });

        Ok(UdpSink { tx })
    }
}

impl Sink<(PacketKind, SocketAddr)> for UdpSink {
    type Error = anyhow::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::poll_ready(Pin::new(&mut self.tx), cx).map_err(anyhow::Error::from)
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        item: (PacketKind, SocketAddr),
    ) -> Result<(), Self::Error> {
        Sink::start_send(Pin::new(&mut self.tx), item).map_err(anyhow::Error::from)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::poll_flush(Pin::new(&mut self.tx), cx).map_err(anyhow::Error::from)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::poll_close(Pin::new(&mut self.tx), cx).map_err(anyhow::Error::from)
    }
}

impl Drop for UdpSink {
    fn drop(&mut self) {
        self.tx.close_channel();
    }
}
