use anyhow::bail;
use chrono::Utc;
use futures::channel::mpsc;
use futures::prelude::*;
use metrics::counter;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio_util::codec::{Decoder, Encoder};

use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{BytesMut, PacketKind, MAX_PACKET_SIZE};
use ya_relay_stack::packet::{ETHERNET_HDR_SIZE, IP6_HDR_SIZE, UDP_HDR_SIZE};

use crate::utils::parse_udp_url;

pub const MTU_ENV_VAR: &str = "YA_NET_MTU";
pub const DEFAULT_MTU: usize = 1500;

pub type InStream =
    Pin<Box<dyn Stream<Item = (PacketKind, SocketAddr, chrono::DateTime<chrono::Utc>)>>>;
pub type OutStream = mpsc::Sender<(PacketKind, SocketAddr)>;

pub async fn udp_bind(addr: &url::Url) -> anyhow::Result<(InStream, OutStream, SocketAddr)> {
    let sock = Arc::new(UdpSocket::bind(&parse_udp_url(addr)?).await?);
    let addr = sock.local_addr()?;

    log::info!("Server listening on: {}", addr);

    let stream = Box::pin(udp_stream(sock.clone()));
    let sink = udp_sink(sock)?;

    Ok((stream, sink, addr))
}

pub fn udp_stream(
    socket: Arc<UdpSocket>,
) -> impl Stream<Item = (PacketKind, SocketAddr, chrono::DateTime<chrono::Utc>)> {
    stream::unfold(socket, |socket| async {
        const MAX_SIZE: usize = MAX_PACKET_SIZE as usize;

        let mut codec = Codec;
        let mut buf = BytesMut::with_capacity(MAX_SIZE);
        // looping till Some is returned to avoid server shutdown (as None triggers server shutdown)
        loop {
            if buf.capacity() == MAX_SIZE {
                // reverts `truncate` only if capacity remained the same
                unsafe {
                    buf.set_len(MAX_SIZE);
                }
            } else {
                buf.resize(MAX_SIZE, 0);
            }

            let (size, addr, timestamp) = match socket.recv_from(&mut buf).await {
                Ok((size, addr)) => (size, addr, Utc::now()),
                Err(e) => {
                    log::warn!("UDP socket error: {}", e);
                    continue;
                }
            };

            buf.truncate(size);
            counter!("ya-relay-core.packet.incoming.size", buf.len() as u64);

            match codec.decode(&mut buf) {
                Ok(Some(item)) => return Some(((item, addr, timestamp), socket)),
                Err(e) => log::warn!("Failed to decode packet from: {addr}. Error: {}", e),
                // `decode` does not return this variant
                Ok(None) => log::warn!("Unable to decode packet of size: {size}, from: {addr}"),
            }
        }
    })
}

pub fn udp_sink(socket: Arc<UdpSocket>) -> anyhow::Result<mpsc::Sender<(PacketKind, SocketAddr)>> {
    let (tx, mut rx) = mpsc::channel(100);

    tokio::task::spawn_local(async move {
        let mut codec = Codec;
        let mut buf = BytesMut::with_capacity(MAX_PACKET_SIZE as usize);
        let max_size = resolve_max_payload_size().await.unwrap();

        while let Some((packet, target)) = rx.next().await {
            // Reusing the buffer, clear existing contents
            buf.clear();

            match &packet {
                PacketKind::Packet(pkt) => {
                    log::trace!(
                        "[udp_sink]: packet: sessionId: [{}], kind: {:?}",
                        hex::encode(&pkt.session_id),
                        pkt.kind
                    );
                }
                PacketKind::Forward(fwd) => {
                    log::trace!(
                        "[udp_sink]: forward: sessionId: [{}], slot: {}, payload-len: {}",
                        hex::encode(&fwd.session_id),
                        fwd.slot,
                        fwd.payload.len()
                    );
                }
                PacketKind::ForwardCtd(_) => {
                    log::trace!("[udp_sink]: forwardCtd");
                }
            }

            if let Err(e) = codec.encode(packet, &mut buf) {
                log::warn!("Error encoding packet for: {target}. Error: {e}");
            } else if let Err(e) = {
                if buf.len() > max_size {
                    log::warn!(
                        "Sending packet of size {} > {max_size} B (soft max) to {target}",
                        buf.len()
                    );
                }

                counter!("ya-relay-core.packet.outgoing.size", buf.len() as u64);
                socket.send_to(&buf, &target).await
            } {
                log::warn!("Error sending packet: {e}");
            }
        }

        log::debug!(
            "Udp socket ({}) closed",
            socket
                .local_addr()
                .map(|addr| addr.to_string())
                .unwrap_or("None".to_string())
        )
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
