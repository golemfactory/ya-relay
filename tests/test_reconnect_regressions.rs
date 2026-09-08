use std::time::Duration;
use ya_relay_client::testing::init::MockSessionNetwork;
use ya_relay_client::testing::private::{SessionError, SessionLock};
use ya_relay_core::server_session::Endpoint;
use ya_relay_core::NodeId;
use ya_relay_server::testing::server::init_test_server;

#[actix_rt::test]
async fn late_close_does_not_disconnect_replacement_relay_session() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let node = network.new_layer().await.unwrap();
    let old = node.layer.server_session().await.unwrap();
    node.layer.close_session(old.clone()).await.unwrap();
    let replacement = node.layer.server_session().await.unwrap();
    assert!(!std::sync::Arc::ptr_eq(&old, &replacement));
    // Within one epoch the relay can assign the exact same wire ID again.
    node.layer.close_session(old).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    replacement.raw.ping().await.unwrap();
    assert!(std::sync::Arc::ptr_eq(
        &node
            .layer
            .find_session(replacement.raw.remote)
            .await
            .unwrap(),
        &replacement,
    ));
}

#[actix_rt::test]
async fn review_old_initialization_must_not_remove_reconnect() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let a = network.new_layer().await.unwrap();
    let id = NodeId::from([2; 20]);
    let mut old = match a.guards.lock_outgoing(id, &[], a.layer.clone()).await {
        SessionLock::Permit(p) => p,
        _ => panic!("missing first permit"),
    };
    a.layer.disconnect(id).await.unwrap();
    let new = match a.guards.lock_outgoing(id, &[], a.layer.clone()).await {
        SessionLock::Permit(p) => p,
        _ => panic!("missing reconnect permit"),
    };
    let _ = old.collect_results(Err(SessionError::Generic("old attempt timed out".into())));
    drop(old);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        a.guards.get_entry(id).await.is_some(),
        "old attempt removed new registry entry"
    );
    drop(new);
}

#[actix_rt::test]
async fn review_stale_close_must_not_remove_replacement_session() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let a = network.new_layer().await.unwrap();
    let b = network.new_layer().await.unwrap();
    b.layer.server_session().await.unwrap();
    a.layer.session(b.id).await.unwrap();
    b.layer.session(a.id).await.unwrap();
    let old = a.layer.find_session(b.addr).await.unwrap();
    a.layer.close_session(old.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    a.layer.session(b.id).await.unwrap();
    let replacement = a.layer.find_session(b.addr).await.unwrap();
    assert_ne!(old.raw.id, replacement.raw.id);
    a.layer.close_session(old).await.unwrap();
    assert_eq!(
        a.layer.find_session(b.addr).await.map(|s| s.raw.id),
        Some(replacement.raw.id),
        "late close of old session removed replacement session"
    );
}

#[actix_rt::test]
async fn review_second_public_endpoint_must_be_attempted() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let a = network.new_layer().await.unwrap();
    let b = network.new_layer().await.unwrap();
    b.layer.server_session().await.unwrap();
    let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut info = a.layer.query_node_info(b.id).await.unwrap();
    info.endpoints = vec![
        Endpoint {
            protocol: ya_relay_proto::proto::Protocol::Udp,
            address: blackhole.local_addr().unwrap(),
        },
        Endpoint {
            protocol: ya_relay_proto::proto::Protocol::Udp,
            address: b.addr,
        },
    ];
    a.guards.update_entry(info).await.unwrap();
    let mut permit = a.start_session(&b).await.unwrap();
    let result = a.layer.try_direct_session(b.id, &permit).await;
    let succeeded = result.is_ok();
    let _ = permit.collect_results(result);
    assert!(
        succeeded,
        "first endpoint timeout prevented connection to live second endpoint"
    );
}

#[actix_rt::test]
async fn review_unknown_session_ping_succeeds_without_peer_session() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let a = network.new_layer().await.unwrap();
    let b = network.new_layer().await.unwrap();
    let raw = a.protocol.temporary_session(&b.addr);
    raw.ping().await.unwrap();
    assert!(b.guards.get_entry(a.id).await.is_none());
    assert!(b.layer.find_session(a.addr).await.is_none());
}

#[actix_rt::test]
async fn review_relay_reconnect_with_alias_must_recover() {
    use ya_relay_client::{ClientBuilder, FailFast};
    use ya_relay_core::crypto::{CryptoProvider, FallbackCryptoProvider};
    use ya_relay_core::testing::TestServerWrapper;
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let a = network.new_layer().await.unwrap();
    let mut crypto = FallbackCryptoProvider::default();
    crypto.add(ya_relay_core::key::generate());
    let alias = crypto.aliases().await.unwrap()[0];
    let b = ClientBuilder::from_url(network.server.url())
        .crypto(crypto)
        .connect(FailFast::Yes)
        .build()
        .await
        .unwrap();
    network.server.remove_node_endpoints(b.node_id()).await;
    a.layer.set_public_addr(None).await;
    // Create A's relay session first, then suppress its public-address reverse path.
    let relay = a.layer.server_session().await.unwrap();
    a.layer.set_public_addr(None).await;
    let route = a.layer.session(alias).await.unwrap();
    assert_eq!(route.route(), Some(NodeId::default()));
    a.layer.close_session(relay.clone()).await.unwrap();
    drop(relay);
    tokio::time::sleep(Duration::from_millis(30)).await;
    a.layer.server_session().await.unwrap();
    a.layer.set_public_addr(None).await;
    let outcome = tokio::time::timeout(Duration::from_secs(4), a.layer.session(b.node_id())).await;
    match outcome {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => panic!("alias retained stale Established state after relay loss: {e}"),
        Err(e) => panic!("alias reconnect exceeded deadline: {e}"),
    }
}

#[actix_rt::test]
async fn review_lost_relay_state_with_continuous_traffic_must_invalidate_session() {
    use ya_relay_client::testing::accessors::ClientPrivate;
    use ya_relay_client::{ClientBuilder, FailFast};
    use ya_relay_core::server_session::TransportType;
    use ya_relay_core::testing::TestServerWrapper;
    let server = init_test_server().await.unwrap();
    let a = ClientBuilder::from_url(server.url())
        .connect(FailFast::No)
        .expire_session_after(Duration::from_millis(200))
        .build()
        .await
        .unwrap();
    let b = ClientBuilder::from_url(server.url())
        .connect(FailFast::Yes)
        .build()
        .await
        .unwrap();
    let layer = a.get_session_layer();
    server.remove_node_endpoints(b.node_id()).await;
    layer.set_public_addr(None).await;
    let mut route = layer.session(b.node_id()).await.unwrap();
    assert_eq!(route.route(), Some(NodeId::default()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let relay = layer.server_session().await.unwrap();
    let old_id = relay.raw.id;
    server.server.sessions().remove_session(&old_id);
    let until = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < until {
        let _ = route
            .send(vec![1u8].into(), TransportType::Unreliable)
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The relay intentionally reuses the wire ID for the same endpoint and epoch.
    // Recovery must replace the local session object, not necessarily its wire ID.
    assert!(
        !layer
            .find_session(relay.raw.remote)
            .await
            .is_some_and(|current| std::sync::Arc::ptr_eq(&current, &relay)),
        "Disconnected packets failed to invalidate the obsolete relay session"
    );
}

#[actix_rt::test]
async fn review_real_relay_restart_recovers_without_continuous_traffic() {
    use ya_relay_client::testing::accessors::ClientPrivate;
    use ya_relay_client::{ClientBuilder, FailFast};
    use ya_relay_core::server_session::TransportType;
    use ya_relay_core::testing::TestServerWrapper;
    use ya_relay_server::testing::server::{init_test_server_with_config, test_default_config};
    let server = init_test_server().await.unwrap();
    let addr = server.server.bind_addr();
    let a = ClientBuilder::from_url(server.url())
        .connect(FailFast::No)
        .expire_session_after(Duration::from_millis(200))
        .build()
        .await
        .unwrap();
    let b = ClientBuilder::from_url(server.url())
        .connect(FailFast::No)
        .expire_session_after(Duration::from_millis(200))
        .build()
        .await
        .unwrap();
    let layer = a.get_session_layer();
    server.remove_node_endpoints(b.node_id()).await;
    layer.set_public_addr(None).await;
    let mut route = layer.session(b.node_id()).await.unwrap();
    assert_eq!(route.route(), Some(NodeId::default()));
    let mut receiver = b.forward_receiver().await.unwrap();
    route
        .send(vec![1u8].into(), TransportType::Unreliable)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    drop(server);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut config = test_default_config();
    config.server.address = addr;
    let restarted = init_test_server_with_config(config).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sessions = restarted.server.sessions();
            if sessions.node_session(a.node_id()).is_some()
                && sessions.node_session(b.node_id()).is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("clients did not register on restarted relay");
    restarted.remove_node_endpoints(b.node_id()).await;
    layer.set_public_addr(None).await;
    route
        .send(vec![2u8].into(), TransportType::Unreliable)
        .await
        .unwrap();
    let packet = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(packet.payload.as_ref(), [2u8]);
}

#[actix_rt::test]
async fn review_absent_peer_can_still_have_established_relay_routing() {
    use ya_relay_client::testing::accessors::SessionLayerPrivate;
    use ya_relay_core::server_session::TransportType;
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let a = network.new_layer().await.unwrap();
    let b = network.new_layer().await.unwrap();
    b.layer.server_session().await.unwrap();
    a.layer.server_session().await.unwrap();
    network.hack_make_layer_ip_private(&b).await;
    a.layer.set_public_addr(None).await;
    b.layer.disable();
    let mut receiver = b.layer.receiver().unwrap();
    let mut route = a.layer.session(b.id).await.unwrap();
    assert_eq!(route.route(), Some(NodeId::default()));
    route
        .send(vec![1u8].into(), TransportType::Unreliable)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), receiver.recv())
            .await
            .is_err()
    );
}

#[actix_rt::test]
async fn review_peer_restart_rotates_key_and_next_datagram_recovers() {
    use ya_relay_client::testing::accessors::ClientPrivate;
    use ya_relay_client::{ClientBuilder, FailFast};
    use ya_relay_core::crypto::FallbackCryptoProvider;
    use ya_relay_core::server_session::TransportType;
    use ya_relay_core::testing::TestServerWrapper;
    let server = init_test_server().await.unwrap();
    let crypto = FallbackCryptoProvider::default();
    let a = ClientBuilder::from_url(server.url())
        .connect(FailFast::Yes)
        .build()
        .await
        .unwrap();
    let mut b = ClientBuilder::from_url(server.url())
        .crypto(crypto.clone())
        .connect(FailFast::Yes)
        .build()
        .await
        .unwrap();
    let layer = a.get_session_layer();
    server.remove_node_endpoints(b.node_id()).await;
    layer.set_public_addr(None).await;
    let old_key = b.get_session_layer().config.session_crypto.pub_key();
    let mut route = layer.session(b.node_id()).await.unwrap();
    assert_eq!(route.route(), Some(NodeId::default()));
    let mut old_receiver = b.forward_receiver().await.unwrap();
    route
        .send(vec![1u8].into(), TransportType::Unreliable)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), old_receiver.recv())
        .await
        .unwrap()
        .unwrap();
    b.shutdown().await.unwrap();
    let b = ClientBuilder::from_url(server.url())
        .crypto(crypto)
        .connect(FailFast::Yes)
        .build()
        .await
        .unwrap();
    assert_ne!(
        old_key.bytes(),
        b.get_session_layer()
            .config
            .session_crypto
            .pub_key()
            .bytes()
    );
    server.remove_node_endpoints(b.node_id()).await;
    let mut receiver = b.forward_receiver().await.unwrap();
    route
        .send(vec![2u8].into(), TransportType::Unreliable)
        .await
        .unwrap();
    let recovered = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            route
                .send(vec![3u8].into(), TransportType::Unreliable)
                .await
                .unwrap();
            if let Ok(Some(packet)) =
                tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await
            {
                if packet.payload.as_ref() == [3u8] {
                    break packet;
                }
            }
        }
    })
    .await
    .expect("encryption did not recover after peer key rotation");
    assert_eq!(recovered.payload.as_ref(), [3u8]);
}
