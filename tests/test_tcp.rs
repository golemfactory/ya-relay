use bytes::BytesMut;
use smoltcp::wire::TcpControl;
use test_log::test;

mod common;

use ya_relay_client::model::Payload;
use ya_relay_client::testing::accessors::{ClientPrivate, TcpSenderPrivate};
use ya_relay_client::testing::init::MockSessionNetwork;
use ya_relay_client::GenericSender;
use ya_relay_core::server_session::TransportType;
use ya_relay_server::testing::server::init_test_server;

use crate::common::tcp::{
    create_ack_for_packet, create_packet, packet_to_buffer, parse_tcp_from_ip6,
};
use common::tcp::create_syn_packet;

/// Hypothesis: Sending SYN packet without continuing the handshake could break smoltcp stack.
/// Smoltcp listening sockets need to be re-bound each time after a new connection will be
/// established. Test checks if the stack is able to handle situation, when nothing after initial
/// SYN packet is sent.
#[test(actix_rt::test)]
async fn test_tcp_syn_packet_unfinished_handshake() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();

    let client1 = network.new_client().await.unwrap();
    let client2 = network.new_client().await.unwrap();
    let layer1 = network.new_layer().await.unwrap();

    // Register nodes on relay
    layer1.layer.server_session().await.unwrap();

    log::info!(
        "== Create session between {} -> {}",
        layer1.id,
        client1.node_id()
    );
    let mut session = layer1.layer.session(client1.node_id()).await.unwrap();

    log::info!("== Send destructive packet to {}", client1.node_id());

    // This packet should break the client.
    let packet =
        create_syn_packet(layer1.id, 5555, client1.node_id(), TransportType::Reliable).unwrap();
    session.send(packet, TransportType::Reliable).await.unwrap();

    let mut receiver = layer1.layer.receiver().unwrap();
    let packet = receiver.recv().await.unwrap();

    log::info!("== Received SYN/ACK packet.");

    let tcp = parse_tcp_from_ip6(&packet.payload).unwrap();
    assert_eq!(tcp.control, smoltcp::wire::TcpControl::Syn);
    assert_eq!(tcp.ack_number, Some(smoltcp::wire::TcpSeqNumber(1)));

    log::info!(
        "== Create session between {} -> {}",
        client2.node_id(),
        client1.node_id()
    );
    let mut sender = client2.forward_reliable(client1.node_id()).await.unwrap();
    sender
        .send(Payload::BytesMut(BytesMut::zeroed(10)))
        .await
        .unwrap();
}

#[test(actix_rt::test)]
async fn test_tcp_syn_in_established_state() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();

    let client1 = network.new_client().await.unwrap();
    let layer1 = network.new_layer().await.unwrap();

    // Register nodes on relay
    layer1.layer.server_session().await.unwrap();

    log::info!(
        "== Create session between {} -> {}",
        layer1.id,
        client1.node_id()
    );
    let mut session = layer1.layer.session(client1.node_id()).await.unwrap();

    log::info!("== Send SYN packet to {}", client1.node_id());

    let packet =
        create_syn_packet(layer1.id, 5555, client1.node_id(), TransportType::Reliable).unwrap();
    session.send(packet, TransportType::Reliable).await.unwrap();

    log::info!("== Waiting for SYN-ACK packet");

    let mut receiver = layer1.layer.receiver().unwrap();
    let packet = receiver.recv().await.unwrap();

    let tcp = parse_tcp_from_ip6(&packet.payload).unwrap();
    assert_eq!(tcp.control, smoltcp::wire::TcpControl::Syn);
    assert_eq!(tcp.ack_number, Some(smoltcp::wire::TcpSeqNumber(1)));

    log::info!("== Received SYN/ACK packet.");
    log::info!("== Sending SYN/ACK packet (last handshake packet).");

    let packet = create_ack_for_packet(layer1.id, client1.node_id(), &tcp).unwrap();
    session.send(packet, TransportType::Reliable).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    log::info!(
        "== Send another SYN packet initiating connection to {}",
        client1.node_id()
    );

    let packet =
        create_syn_packet(layer1.id, 5555, client1.node_id(), TransportType::Reliable).unwrap();
    session.send(packet, TransportType::Reliable).await.unwrap();

    let packet = receiver.recv().await.unwrap();
    let tcp = parse_tcp_from_ip6(&packet.payload).unwrap();
    log::info!("== Received packet: {tcp}");
    assert_eq!(tcp.control, smoltcp::wire::TcpControl::Rst);
}

#[test(actix_rt::test)]
async fn test_tcp_rst_when_socket_doesnt_listen() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();

    let client1 = network.new_client().await.unwrap();
    let layer1 = network.new_layer().await.unwrap();

    // Register nodes on relay
    layer1.layer.server_session().await.unwrap();

    log::info!(
        "== Create session between {} -> {}",
        layer1.id,
        client1.node_id()
    );
    let mut session = layer1.layer.session(client1.node_id()).await.unwrap();

    log::info!(
        "== Send SYN packet to {} with unexpected port",
        client1.node_id()
    );

    let (ip_repr, tcp_repr) = create_packet(layer1.id, 5555, client1.node_id(), 3).unwrap();
    let packet = Payload::BytesMut(packet_to_buffer(&ip_repr, &tcp_repr));

    session.send(packet, TransportType::Reliable).await.unwrap();

    log::info!("== Waiting for RST packet");

    let mut receiver = layer1.layer.receiver().unwrap();
    let packet = receiver.recv().await.unwrap();

    let tcp = parse_tcp_from_ip6(&packet.payload).unwrap();

    log::info!("== Received packet: {tcp}");
    assert_eq!(tcp.control, smoltcp::wire::TcpControl::Rst);
}

#[test(actix_rt::test)]
async fn test_tcp_exploit_remove_listening_socket() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();

    let client1 = network.new_client().await.unwrap();
    let client2 = network.new_client().await.unwrap();
    let client3 = network.new_client().await.unwrap();

    log::info!(
        "== Create TCP connection between {} -> {} and reverse",
        client1.node_id(),
        client2.node_id()
    );

    // Create connections in both directions.
    let _tcp1 = client1.forward_reliable(client2.node_id()).await.unwrap();
    let mut tcp2 = client2.forward_reliable(client1.node_id()).await.unwrap();

    // Disconnect one-sided connection from client1. The second connection will be closed as a result,
    // but a little bit later, and it will be triggered by code on client1.
    let layer2 = client2.get_session_layer();
    let mut session = layer2.session(client1.node_id()).await.unwrap();
    let tcp_sender = client2.forward_reliable(client1.node_id()).await.unwrap();

    let src_port = tcp_sender.get_local_addr().unwrap().port;
    let dst_port = tcp_sender.get_remote_addr().unwrap().port;

    let (ip_repr, mut tcp_repr) =
        create_packet(client2.node_id(), src_port, client1.node_id(), dst_port).unwrap();
    tcp_repr.control = TcpControl::Rst;
    let packet = Payload::BytesMut(packet_to_buffer(&ip_repr, &tcp_repr));
    session.send(packet, TransportType::Reliable).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Connect new Node to client1. This should use current listening socket which needs to be replaced.
    // Since we freed outgoing socket by disconnecting, the socket handle will be reused.
    client3.forward_reliable(client1.node_id()).await.unwrap();

    assert!(tcp2.connect().await.is_err());
}
