use anyhow::bail;
use futures::channel::mpsc;
use futures::prelude::*;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::net::udp::{RecvHalf, SendHalf};
use tokio::net::UdpSocket;
use tokio_util::codec::{Decoder, Encoder};

use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{BytesMut, PacketKind, MAX_PACKET_SIZE};
use ya_relay_stack::packet::{ETHERNET_HDR_SIZE, IP6_HDR_SIZE, UDP_HDR_SIZE};

use crate::utils::parse_udp_url;

pub const MTU_ENV_VAR: &str = "YA_NET_MTU";
pub const DEFAULT_MTU: usize = 1500;
pub const DEFAULT_MAX_PAYLOAD_SIZE: usize = 1280;

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

pub fn udp_stream(socket: RecvHalf) -> impl Stream<Item = (PacketKind, SocketAddr)> {
    stream::unfold(socket, |mut socket| async {
        let mut codec = Codec::default();
        let mut buf = BytesMut::with_capacity(MAX_PACKET_SIZE as usize);

        // looping till Some is returned to avoid server shutdown (as None triggers server shutdown)
        loop {
            buf.resize(MAX_PACKET_SIZE as usize, 0);

            let (size, addr) = match socket.recv_from(&mut buf).await {
                Ok((size, addr)) => (size, addr),
                Err(e) => {
                    log::warn!("UDP socket error: {}", e);
                    continue;
                }
            };

            buf.truncate(size);

            match codec.decode(&mut buf) {
                Ok(Some(item)) => return Some(((item, addr), socket)),
                Err(e) => log::warn!("Failed to decode packet: {}", e),
                // `decode` does not return this variant
                Ok(None) => log::warn!("Unable to decode packet"),
            }
        }
    })
}

pub fn udp_sink(mut socket: SendHalf) -> anyhow::Result<mpsc::Sender<(PacketKind, SocketAddr)>> {
    let (tx, mut rx) = mpsc::channel(20);

    tokio::task::spawn_local(async move {
        let mut codec = Codec::default();
        let mut buf = BytesMut::with_capacity(MAX_PACKET_SIZE as usize);
        let max_size = resolve_max_payload_size().await.unwrap();

        while let Some((packet, target)) = rx.next().await {
            // Reusing the buffer, clear existing contents
            buf.clear();

            if let Err(e) = codec.encode(packet, &mut buf) {
                log::warn!("Error encoding packet: {}", e);
            } else if let Err(e) = {
                if buf.len() > max_size {
                    log::warn!(
                        "Sending packet of size {} > {} B (soft max) to {}",
                        buf.len(),
                        max_size,
                        target
                    );
                }
                socket.send_to(&buf, &target).await
            } {
                log::warn!("Error sending packet: {}", e);
            }
        }
    });

    Ok(tx)
}

pub async fn resolve_max_payload_size() -> anyhow::Result<usize> {
    resolve_max_payload_overhead_size(0).await
}

pub async fn resolve_max_payload_overhead_size(overhead: usize) -> anyhow::Result<usize> {
    const OVERHEAD: usize = ETHERNET_HDR_SIZE + IP6_HDR_SIZE + UDP_HDR_SIZE;

    let minimum = OVERHEAD + overhead + 1;
    let mut mtu = std::env::var(MTU_ENV_VAR)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MTU);

    if mtu < minimum {
        let err = format!("MTU value {mtu} is below {minimum}");
        if DEFAULT_MTU < minimum {
            bail!(err);
        }
        log::warn!("{err}, reverting to {DEFAULT_MTU}");
        mtu = DEFAULT_MTU;
    }

    Ok(mtu - OVERHEAD - overhead)
}
