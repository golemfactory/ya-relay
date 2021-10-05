use anyhow::anyhow;
use bytes::BytesMut;
use futures::channel::mpsc;
use futures::prelude::*;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::net::udp::{RecvHalf, SendHalf};
use tokio::net::UdpSocket;
use tokio_util::codec::{Decoder, Encoder};

use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{PacketKind, MAX_PACKET_SIZE};

use crate::parse_udp_url;

pub type InStream = Pin<Box<dyn Stream<Item = (PacketKind, SocketAddr)>>>;
pub type OutStream = mpsc::Sender<(PacketKind, SocketAddr)>;

pub async fn udp_bind(addr: &url::Url) -> anyhow::Result<(InStream, OutStream, SocketAddr)> {
    let sock = UdpSocket::bind(&parse_udp_url(addr)?).await?;
    let addr = sock.local_addr()?;

    log::info!("Server listening on: {}", addr);

    let (input, output) = sock.split();

    let stream = Box::pin(udp_stream(input));
    let sink = udp_sink(output)?;

    Ok((stream, sink, addr))
}

pub async fn receive_packet(socket: &mut RecvHalf) -> anyhow::Result<(PacketKind, SocketAddr)> {
    let mut codec = Codec::default();
    let mut buf = BytesMut::new();

    buf.resize(MAX_PACKET_SIZE as usize, 0);

    let (size, addr) = socket.recv_from(&mut buf).await?;

    log::trace!("Received {} bytes from {}", size, addr);

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

pub fn udp_sink(mut socket: SendHalf) -> anyhow::Result<mpsc::Sender<(PacketKind, SocketAddr)>> {
    let (tx, mut rx) = mpsc::channel(20);

    tokio::task::spawn_local(async move {
        let mut codec = Codec::default();
        let mut buf = BytesMut::with_capacity(MAX_PACKET_SIZE as usize);

        while let Some((packet, target)) = rx.next().await {
            // Reusing the buffer, clear existing contents
            buf.clear();

            if let Err(e) = codec.encode(packet, &mut buf) {
                log::warn!("Error encoding packet. {}", e);
            } else {
                // Error can happen only, if IP version of socket doesn't match,
                // So we can ignore it.
                socket.send_to(&buf, &target).await.ok();
            }
        }
    });

    Ok(tx)
}
