use anyhow::bail;
use bytes::BytesMut;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{IpProtocol, IpRepr, TcpControl, TcpSeqNumber};
use smoltcp::wire::{TcpPacket, TcpRepr, TCP_HEADER_LEN};
use std::mem;

use ya_relay_client::model::Payload;
use ya_relay_client::testing::private::{ChannelType, VirtNode};
use ya_relay_core::server_session::TransportType;
use ya_relay_core::NodeId;
use ya_relay_stack::StackConfig;

pub fn syn_packet(
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

    let ip_repr = IpRepr::new(local, remote, IpProtocol::Tcp, 0, 64);

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

    let mut buffer = BytesMut::zeroed(tcp_repr.buffer_len());
    let mut packet = TcpPacket::new_unchecked(&mut buffer);
    tcp_repr.emit(
        &mut packet,
        &local,
        &remote,
        &ChecksumCapabilities::default(),
    );

    Ok(Payload::BytesMut(buffer))
}
