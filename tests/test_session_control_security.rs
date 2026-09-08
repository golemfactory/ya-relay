use std::time::Duration;

use ya_relay_client::testing::init::MockSessionNetwork;
use ya_relay_core::server_session::SessionId;
use ya_relay_proto::proto::{control::disconnected::By, control::Disconnected, Packet};
use ya_relay_server::testing::server::init_test_server;

#[actix_rt::test]
async fn unknown_sender_cannot_disconnect_another_node() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let victim = network.new_layer().await.unwrap();
    let peer = network.new_layer().await.unwrap();
    let attacker = network.new_layer().await.unwrap();
    peer.layer.server_session().await.unwrap();
    victim.layer.session(peer.id).await.unwrap();
    peer.layer.session(victim.id).await.unwrap();
    let original = victim.layer.find_session(peer.addr).await.unwrap().raw.id;

    attacker
        .protocol
        .temporary_session(&victim.addr)
        .send(Packet::control(
            SessionId::generate().to_vec(),
            Disconnected {
                by: Some(By::NodeId(peer.id.into_array().to_vec())),
            },
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        victim.layer.find_session(peer.addr).await.map(|s| s.raw.id),
        Some(original)
    );
}

#[actix_rt::test]
async fn authenticated_peer_cannot_disconnect_another_node() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let victim = network.new_layer().await.unwrap();
    let peer = network.new_layer().await.unwrap();
    let attacker = network.new_layer().await.unwrap();
    peer.layer.server_session().await.unwrap();
    victim.layer.session(peer.id).await.unwrap();
    peer.layer.session(victim.id).await.unwrap();
    attacker.layer.server_session().await.unwrap();
    victim.layer.session(attacker.id).await.unwrap();
    attacker.layer.session(victim.id).await.unwrap();
    let original = victim.layer.find_session(peer.addr).await.unwrap().raw.id;
    let attacker_session = attacker.layer.find_session(victim.addr).await.unwrap();
    attacker_session
        .raw
        .send(Packet::control(
            attacker_session.raw.id.to_vec(),
            Disconnected {
                by: Some(By::NodeId(peer.id.into_array().to_vec())),
            },
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        victim.layer.find_session(peer.addr).await.map(|s| s.raw.id),
        Some(original)
    );
}

#[actix_rt::test]
async fn known_session_id_does_not_authorize_foreign_sender_on_relay() {
    let server = init_test_server().await.unwrap();
    let relay_addr = server.server.bind_addr();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let victim = network.new_layer().await.unwrap();
    let attacker = network.new_layer().await.unwrap();
    let session = victim.layer.server_session().await.unwrap();
    let id = session.raw.id;

    attacker
        .protocol
        .temporary_session(&relay_addr)
        .send(Packet::control(
            id.to_vec(),
            Disconnected {
                by: Some(By::SessionId(id.to_vec())),
            },
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    session.raw.ping().await.unwrap();

    // Even the owner must identify the same session in the header and body.
    session
        .raw
        .send(Packet::control(
            id.to_vec(),
            Disconnected {
                by: Some(By::SessionId(SessionId::generate().to_vec())),
            },
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    session.raw.ping().await.unwrap();

    // The actual owner must still be able to disconnect its session.
    session.raw.disconnect().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(session.raw.ping().await.is_err());
}
