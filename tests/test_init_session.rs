mod common;

use std::convert::TryInto;
use std::time::Duration;
use ya_relay_core::server_session::SessionId;

use ya_relay_client::testing::init::MockSessionNetwork;
use ya_relay_client::testing::private::SessionLock;
use ya_relay_client::testing::private::SessionState;
use ya_relay_client::ClientBuilder;
use ya_relay_client::SessionError;
use ya_relay_core::testing::TestServerWrapper;
use ya_relay_proto::proto;
use ya_relay_server::testing::server::init_test_server;

#[actix_rt::test]
async fn test_session_protocol_happy_path() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
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
async fn test_session_protocol_invalid_challenge() {
    scripted_failure(HandshakeFailure::InvalidChallenge).await;
}

/// Initialization should be rejected if Node responds with different id, than
/// NodeId we intended to connect to.
#[actix_rt::test]
async fn test_session_protocol_node_id_mismatch() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let a = network.new_layer().await.unwrap();
    let b = network.new_layer().await.unwrap();
    let wrong_id = ya_relay_core::NodeId::from([7; 20]);
    let mut permit = match a
        .guards
        .lock_outgoing(wrong_id, &[b.addr], a.layer.clone())
        .await
    {
        SessionLock::Permit(permit) => permit,
        _ => panic!("expected permit"),
    };
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        a.protocol.init_p2p_session(b.addr, &permit),
    )
    .await
    .expect("identity validation must finish")
    .map_err(SessionError::from);
    let error = permit
        .collect_results(result)
        .err()
        .expect("wrong identity must be rejected");
    assert!(
        error.to_string().contains("Invalid default NodeId"),
        "{error}"
    );
    drop(permit);
    assert!(a.protocol.get_temporary_session(&b.addr).is_none());
    assert!(a.layer.find_session(b.addr).await.is_none());
}

/// `SessionProtocol` should correctly handle situation, when we didn't get response
/// to initial message.
#[actix_rt::test]
async fn test_session_protocol_handshake_timeout() {
    scripted_failure(HandshakeFailure::HelloTimeout).await;
}

/// `SessionProtocol` should correctly handle situation, when we didn't get response
/// with challenge solution.
#[actix_rt::test]
async fn test_session_protocol_challenge_handshake_timeout() {
    scripted_failure(HandshakeFailure::ChallengeTimeout).await;
}

/// `SessionLayer` should react correctly, when gets Disconnected message
/// after sending first handshake.
#[actix_rt::test]
async fn test_session_protocol_disconnected_on_handshake() {
    scripted_failure(HandshakeFailure::DisconnectHello).await;
}

/// `SessionLayer` should react correctly, when gets Disconnected message
/// after sending challenge handshake.
#[actix_rt::test]
async fn test_session_protocol_disconnected_on_challenge_response() {
    scripted_failure(HandshakeFailure::DisconnectChallenge).await;
}

#[derive(Clone, Copy)]
enum HandshakeFailure {
    HelloTimeout,
    ChallengeTimeout,
    InvalidChallenge,
    DisconnectHello,
    DisconnectChallenge,
}

async fn scripted_failure(failure: HandshakeFailure) {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let a = network.new_layer().await.unwrap();
    let b = network.new_layer().await.unwrap();
    let mut requests = b.capturer.captures.session_request.start_capture();
    let initiator = a.clone();
    let target = b.clone();
    let attempt = tokio::task::spawn_local(async move {
        let mut permit = initiator.start_session(&target).await.unwrap();
        let result = permit
            .run_abortable(async {
                initiator
                    .protocol
                    .init_p2p_session(target.addr, &permit)
                    .await
                    .map_err(SessionError::from)
            })
            .await;
        permit.collect_results(result)
    });
    let hello = tokio::time::timeout(Duration::from_secs(1), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(hello.session_id.is_empty());
    let peer = b.protocol.temporary_session(&a.addr);
    let id = SessionId::generate();
    let mut response_request_id = hello.request_id;
    if !matches!(
        failure,
        HandshakeFailure::HelloTimeout | HandshakeFailure::DisconnectHello
    ) {
        let (challenge, _) = ya_relay_core::challenge::prepare_challenge_response(1);
        peer.send(proto::Packet::response(
            hello.request_id,
            id.to_vec(),
            proto::StatusCode::Ok,
            challenge,
        ))
        .await
        .unwrap();
        // Ignore retransmissions of the first handshake if UDP delivery was delayed.
        let second = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let request = requests.recv().await.unwrap();
                if request.request.challenge_resp.is_some() {
                    break request;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(second.session_id.as_slice(), id.as_ref());
        response_request_id = second.request_id;
    }
    match failure {
        HandshakeFailure::InvalidChallenge => {
            peer.send(proto::Packet::response(
                response_request_id,
                id.to_vec(),
                proto::StatusCode::Ok,
                proto::response::Session {
                    challenge_resp: Some(Default::default()),
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
        }
        HandshakeFailure::DisconnectHello | HandshakeFailure::DisconnectChallenge => {
            peer.send(proto::Packet::control(
                id.to_vec(),
                proto::control::Disconnected {
                    by: Some(proto::control::disconnected::By::SessionId(id.to_vec())),
                },
            ))
            .await
            .unwrap();
        }
        _ => (),
    }
    let error = tokio::time::timeout(Duration::from_secs(10), attempt)
        .await
        .expect("failed handshake must terminate")
        .unwrap()
        .err()
        .expect("handshake must fail");
    match failure {
        HandshakeFailure::HelloTimeout | HandshakeFailure::ChallengeTimeout => {
            assert!(error.to_string().contains("timed out"), "{error}");
        }
        HandshakeFailure::InvalidChallenge => {
            assert!(error.to_string().contains("Invalid challenge"), "{error}")
        }
        _ => assert!(matches!(error, SessionError::Aborted(_)), "{error}"),
    }
    assert!(a.protocol.get_temporary_session(&b.addr).is_none());
    assert!(a.layer.find_session(b.addr).await.is_none());
    tokio::time::timeout(Duration::from_secs(1), async {
        while a.guards.get_entry(b.id).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed generation must be retired");
}

#[test_log::test(actix_rt::test)]
async fn test_query_self_node_info() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let client = ClientBuilder::from_url(wrapper.url())
        .build()
        .await
        .unwrap();

    let node_id = client.node_id();
    let endpoints = [proto::Endpoint {
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
