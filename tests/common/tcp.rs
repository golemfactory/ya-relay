use anyhow::bail;
use bytes::BytesMut;
use std::mem;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{IpProtocol, IpRepr, Ipv6Packet, Ipv6Repr, TcpControl, TcpSeqNumber};
use smoltcp::wire::{TcpPacket, TcpRepr, TCP_HEADER_LEN};

use ya_relay_client::model::Payload;
use ya_relay_client::testing::private::{ChannelType, VirtNode};
use ya_relay_core::server_session::TransportType;
use ya_relay_core::NodeId;
use ya_relay_stack::StackConfig;

pub fn create_syn_packet(
    from: NodeId,
    src_port: u16,
    to: NodeId,
    transport: TransportType,
) -> anyhow::Result<Payload> {
    let channel_port = match transport {
        TransportType::Reliable => ChannelType::Messages as u16,
        TransportType::Transfer => ChannelType::Transfer as u16,
        _ => bail!("Syn packet can be sent only for Messages and Transfer transports",),
    };

    let local = VirtNode::ip_from_node_id(from);
    let remote = VirtNode::ip_from_node_id(to);

    let mut ip_repr = IpRepr::new(local, remote, IpProtocol::Tcp, 0, 64);

    // Check crates/stack/src/device.rs line 29
    let max_transmission_unit = 1280;
    let max_segment_size = max_transmission_unit - ip_repr.header_len() - TCP_HEADER_LEN;
    let config = StackConfig::default();

    let rx_cap_log2 = mem::size_of::<usize>() * 8 - config.tcp_mem.tx.max.leading_zeros() as usize;

    let tcp_repr = TcpRepr {
        src_port,
        dst_port: channel_port,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber::default(),
        ack_number: None,
        // Check https://github.com/smoltcp-rs/smoltcp/blob/cfc17ba16d3fdadb3f86e2d5b326af3797284b22/src/socket/tcp.rs#L2158
        window_len: config.tcp_mem.rx.max.min((1 << 16) - 1) as u16,
        // Check https://github.com/smoltcp-rs/smoltcp/blob/cfc17ba16d3fdadb3f86e2d5b326af3797284b22/src/socket/tcp.rs#513
        window_scale: Some(rx_cap_log2.saturating_sub(16) as u8),
        max_seg_size: Some(max_segment_size as u16),
        sack_permitted: true,
        sack_ranges: [None, None, None],
        payload: &[],
    };

    ip_repr.set_payload_len(tcp_repr.buffer_len());
    log::info!("== IP packet: {ip_repr:?}");

    let mut buffer = BytesMut::zeroed(ip_repr.header_len() + tcp_repr.buffer_len());
    ip_repr.emit(&mut buffer, &ChecksumCapabilities::default());

    let mut tcp_packet = TcpPacket::new_unchecked(&mut buffer[ip_repr.header_len()..]);
    log::info!("== TCP packet: {tcp_repr}");

    tcp_repr.emit(
        &mut tcp_packet,
        &local,
        &remote,
        &ChecksumCapabilities::default(),
    );

    Ok(Payload::BytesMut(buffer))
}

pub fn parse_tcp_from_ip6(payload: &Payload) -> anyhow::Result<TcpRepr> {
    let packet = Ipv6Packet::new_unchecked(&payload.as_ref()[..]);
    let ip = IpRepr::Ipv6(Ipv6Repr::parse(&packet)?);

    Ok(TcpRepr::parse(
        &TcpPacket::new_unchecked(&payload.as_ref()[ip.header_len()..]),
        &ip.src_addr(),
        &ip.dst_addr(),
        &ChecksumCapabilities::default(),
    )?)
}
