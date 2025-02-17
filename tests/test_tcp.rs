use test_log::test;

mod common;

use ya_relay_client::testing::init::MockSessionNetwork;
use ya_relay_core::server_session::TransportType;
use ya_relay_server::testing::server::init_test_server;

use crate::common::tcp::parse_tcp_from_ip6;
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
    client2.forward_reliable(client1.node_id()).await.unwrap();
}
