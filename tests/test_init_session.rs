mod common;

use std::convert::TryInto;

use ya_relay_client::ClientBuilder;
use ya_relay_proto::proto;
use common::server::init_test_server;
use ya_relay_client::network_view::SessionLock;
use ya_relay_client::session_state::SessionState;
use common::init::MockSessionNetwork;

#[actix_rt::test]
async fn test_session_protocol_happy_path() {
    let mut network = MockSessionNetwork::new().await.unwrap();
    let layer1 = network.new_layer().await.unwrap();
    let layer2 = network.new_layer().await.unwrap();
    let protocol = layer1.protocol;

    let mut permit = match layer1
        .guards
        .lock_outgoing(layer2.id, &[layer2.addr], layer1.layer.clone())
        .await
    {
        SessionLock::Permit(permit) => permit,
        SessionLock::Wait(_) => panic!("Expected initialization permit"),
    };

    let guard1 = permit.registry.clone();
    let mut waiter1 = guard1.awaiting_notifier();

    let _ = permit.collect_results(
        protocol
            .init_p2p_session(layer2.addr, &permit)
            .await
            .map_err(SessionError::from),
    );

    // Finishes the initialization by setting established state.
    drop(permit);

    let mut waiter2 = match layer2
        .guards
        .lock_incoming(layer1.id, &[layer1.addr], layer2.layer.clone())
        .await
    {
        SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
        SessionLock::Wait(waiter) => waiter,
    };

    // Wait until both sides will have session established.
    // Notice: We need to wait on waiter1, because `Permit` drop will spawn asynchronous task
    // to make final state change.
    waiter1.await_for_finish().await.unwrap();
    waiter2.await_for_finish().await.unwrap();
    assert!(matches!(
        waiter2.registry.state().await,
        SessionState::Established(..)
    ));
    eprintln!("{}", guard1.state().await);
    assert!(matches!(
        guard1.state().await,
        SessionState::Established(..)
    ));
}

// /// Connection attempt should be rejected, if challenge is not present.
// #[actix_rt::test]
// async fn test_session_protocol_init_without_challenge() {
//     let mut network = MockSessionNetwork::new().await.unwrap();
//     let layer1 = network.new_layer().await.unwrap();
//     let layer2 = network.new_layer().await.unwrap();
//     let protocol1 = layer1.protocol.clone();
//
//     let permit = layer1.start_session(&layer2).await.unwrap();
//     let tmp_session = protocol1.temporary_session(&layer2.addr).await;
//
//     let (request, raw_challenge) = protocol1.prepare_challenge_request(false).await.unwrap();
//     let response = tmp_session
//         .request::<proto::response::Session>(request.into(), vec![], Duration::from_millis(500))
//         .await;
//
//     assert!(response.is_err());
// }

// /// Node is expected to send empty session id to show initialization intent.
// /// Not empty session id should be rejected.
// #[actix_rt::test]
// async fn test_session_protocol_init_not_empty_session_id() {
//     let mut network = MockSessionNetwork::new().await.unwrap();
//     let layer1 = network.new_layer().await.unwrap();
//     let layer2 = network.new_layer().await.unwrap();
//     let protocol1 = layer1.protocol.clone();
//
//     let permit = layer1.start_session(&layer2).await.unwrap();
//     let tmp_session = protocol1.temporary_session(&layer2.addr).await;
//
//     let (request, raw_challenge) = protocol1.prepare_challenge_request(true).await.unwrap();
//     let response = tmp_session
//         .request::<proto::response::Session>(
//             request.into(),
//             SessionId::generate().to_vec(),
//             Duration::from_millis(500),
//         )
//         .await;
//
//     response.unwrap();
//     //assert!(response.is_err());
// }

#[actix_rt::test]
async fn test_session_protocol_invalid_challenge() {}

/// Initialization should be rejected if Node responds with different id, than
/// NodeId we intended to connect to.
#[actix_rt::test]
async fn test_session_protocol_node_id_mismatch() {}

/// `SessionProtocol` should correctly handle situation, when we didn't get response
/// to initial message.
#[actix_rt::test]
async fn test_session_protocol_handshake_timeout() {}

/// `SessionProtocol` should correctly handle situation, when we didn't get response
/// with challenge solution.
#[actix_rt::test]
async fn test_session_protocol_challenge_handshake_timeout() {}

/// `SessionLayer` should react correctly, when gets Disconnected message
/// after sending first handshake.
#[actix_rt::test]
async fn test_session_protocol_disconnected_on_handshake() {}

/// `SessionLayer` should react correctly, when gets Disconnected message
/// after sending challenge handshake.
#[actix_rt::test]
async fn test_session_protocol_disconnected_on_challenge_response() {}

#[serial_test::serial]
async fn test_query_self_node_info() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let client = ClientBuilder::from_url(wrapper.server.inner.url.clone())
        .build()
        .await
        .unwrap();

    let node_id = client.node_id();
    let endpoints = vec![proto::Endpoint {
        protocol: proto::Protocol::Udp as i32,
        address: "127.0.0.1".to_string(),
        port: client.bind_addr().await?.port() as u32,
    }];

    //let session = client .sessions.server_session().await.unwrap();
    let node_info = client.find_node(node_id).await.unwrap();

    // TODO: More checks, after everything will be implemented.
    assert_eq!(
        node_id,
        (&node_info.identities[0].node_id).try_into().unwrap()
    );
    assert_ne!(node_info.slot, u32::MAX);
    assert_eq!(node_info.endpoints.len(), 1);
    assert_eq!(node_info.endpoints[0], endpoints[0]);

    Ok(())
}
