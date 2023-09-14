mod common;

use std::time::Duration;

use tokio::time::timeout;
use ya_relay_core::server_session::TransportType;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::Payload;

use ya_relay_client::testing::accessors::SessionLayerPrivate;
use ya_relay_client::testing::init::MockSessionNetwork;
use ya_relay_client::testing::private::SessionInitializer;
use ya_relay_client::testing::private::SessionLayer;
use ya_relay_client::testing::private::SessionType;
use ya_relay_server::testing::server::init_test_server;

use anyhow::bail;
use futures::future::LocalBoxFuture;
use futures::FutureExt;
use std::net::SocketAddr;

#[actix_rt::test]
async fn test_session_layer_happy_path() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let layer1 = network.new_layer().await.unwrap();
    let layer2 = network.new_layer().await.unwrap();

    // Node-2 should be registered on relay
    layer2.layer.server_session().await.unwrap();
    let session = layer1.layer.session(layer2.id).await.unwrap();

    // p2p session - target and route are the same.
    assert_eq!(session.target(), layer2.id);
    assert_eq!(session.route(), layer2.id);
    assert_eq!(session.session_type(), SessionType::P2P);

    let session = layer2.layer.session(layer1.id).await.unwrap();

    assert_eq!(session.target(), layer1.id);
    assert_eq!(session.route(), layer1.id);
    assert_eq!(session.session_type(), SessionType::P2P);
}

#[actix_rt::test]
async fn test_session_layer_p2p_send_receive() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let layer1 = network.new_layer().await.unwrap();
    let layer2 = network.new_layer().await.unwrap();

    let mut receiver1 = layer1.layer.receiver().unwrap();
    let mut receiver2 = layer2.layer.receiver().unwrap();

    // Node-2 should be registered on relay
    layer2.layer.server_session().await.unwrap();

    // Send Node-1 -> Node-2
    let mut session = layer1.layer.session(layer2.id).await.unwrap();

    let packet = Payload::Vec(vec![4u8]);
    session
        .send(packet.clone(), TransportType::Unreliable)
        .await
        .unwrap();

    let forwarded = timeout(Duration::from_millis(300), receiver2.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(forwarded.node_id, layer1.id);
    assert_eq!(forwarded.transport, TransportType::Unreliable);
    assert_eq!(forwarded.payload, packet);

    // Send Node-2 -> Node-1
    let mut session = layer2.layer.session(layer1.id).await.unwrap();

    let packet = Payload::Vec(vec![7u8]);
    session
        .send(packet.clone(), TransportType::Unreliable)
        .await
        .unwrap();

    let forwarded = timeout(Duration::from_millis(300), receiver1.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(forwarded.node_id, layer2.id);
    assert_eq!(forwarded.transport, TransportType::Unreliable);
    assert_eq!(forwarded.payload, packet);
}

#[actix_rt::test]
async fn test_session_layer_close_p2p_session() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let layer1 = network.new_layer().await.unwrap();
    let layer2 = network.new_layer().await.unwrap();

    // Node-2 should be registered on relay
    layer2.layer.server_session().await.unwrap();
    let mut session = layer1.layer.session(layer2.id).await.unwrap();
    // Wait until second Node will be ready with session
    let session2 = layer2.layer.session(layer1.id).await.unwrap();

    assert_eq!(session.target(), layer2.id);
    assert_eq!(session.route(), layer2.id);

    assert_eq!(session2.target(), layer1.id);
    assert_eq!(session2.route(), layer1.id);

    session.disconnect().await.unwrap();
    // Let other side receive and handle `Disconnected` packet.
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(layer1.layer.get_node_routing(layer2.id).await.is_none());
    assert!(layer2.layer.get_node_routing(layer1.id).await.is_none());

    // We should be able to connect again to the same Node.
    session.connect().await.unwrap();
}

#[actix_rt::test]
async fn test_session_layer_close_relayed_routing() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let layer1 = network.new_layer().await.unwrap();
    let layer2 = network.new_layer().await.unwrap();
    let relay_id = NodeId::default();

    // Node-2 should be registered on relay
    layer1.layer.server_session().await.unwrap();
    layer2.layer.server_session().await.unwrap();
    network.hack_make_layer_ip_private(&layer2).await;
    network.hack_make_layer_ip_private(&layer1).await;

    let mut session = layer1.layer.session(layer2.id).await.unwrap();
    // Wait until second Node will be ready with session
    let session2 = layer2.layer.session(layer1.id).await.unwrap();

    assert_eq!(session.target(), layer2.id);
    assert_eq!(session.route(), relay_id);
    assert_eq!(session.session_type(), SessionType::Relay);

    assert_eq!(session2.target(), layer1.id);
    assert_eq!(session2.route(), relay_id);
    assert_eq!(session2.session_type(), SessionType::Relay);

    session.disconnect().await.unwrap();
    // Let other side receive and handle `Disconnected` packet.
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(layer1.layer.get_node_routing(layer2.id).await.is_none());
    // There is no way Node-2 will know that relayed connection was closed.
    //assert!(layer2.layer.get_node_routing(layer1.id).await.is_none());

    // We should be able to connect again to the same Node.
    session.connect().await.unwrap();
}

#[actix_rt::test]
async fn test_session_layer_reverse_connection() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let layer1 = network.new_layer().await.unwrap();
    let layer2 = network.new_layer().await.unwrap();

    // Node-2 should be registered on relay
    layer2.layer.server_session().await.unwrap();
    network.hack_make_layer_ip_private(&layer2).await;

    let session = layer1.layer.session(layer2.id).await.unwrap();
    // Wait until second Node will be ready with session
    let session2 = layer2.layer.session(layer1.id).await.unwrap();

    assert_eq!(session.target(), layer2.id);
    assert_eq!(session.route(), layer2.id);
    assert_eq!(session.session_type(), SessionType::P2P);

    assert_eq!(session2.target(), layer1.id);
    assert_eq!(session2.route(), layer1.id);
    assert_eq!(session2.session_type(), SessionType::P2P);
}
