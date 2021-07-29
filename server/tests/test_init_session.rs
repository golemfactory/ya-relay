use std::convert::TryFrom;
use std::str::FromStr;

use ya_client_model::node_id::NodeId;
use ya_net_server::testing::server::init_test_server;
use ya_net_server::testing::Client;
use ya_net_server::SessionId;
use ya_relay_proto::proto;

#[serial_test::serial]
async fn test_query_self_node_info() -> anyhow::Result<()> {
    let server = init_test_server().await.unwrap();
    let client = Client::connect(&server).await.unwrap();

    // TODO: Could be generated by `Client`. Note that node id is related to
    //       Client's public/private key. Client should be able to sign with his key.
    let node_id = NodeId::from_str("0x00069fc6fd02afeca110b9c32a21fb8ad899ee0a")?;
    let endpoints = vec![proto::Endpoint {
        protocol: proto::Protocol::Udp as i32,
        address: "127.0.0.1".to_string(),
        port: client.inner.read().await.socket.local_addr()?.port() as u32,
    }];

    let session = client.init_session(node_id).await.unwrap();
    let result_endpoints = client.register_endpoints(session, vec![]).await.unwrap();
    let node_info = client.find_node(session, node_id).await.unwrap();

    // TODO: More checks, after everything will be implemented.
    assert_eq!(node_id, NodeId::from(&node_info.node_id[..]));
    assert_eq!(node_info.random, false);
    assert_eq!(node_info.endpoints.len(), 1);
    assert_eq!(node_info.endpoints[0], endpoints[0]);

    // Check server response.
    assert_eq!(result_endpoints.len(), 1);
    assert_eq!(result_endpoints[0], endpoints[0]);
    Ok(())
}

#[serial_test::serial]
async fn test_request_with_invalid_session() -> anyhow::Result<()> {
    let server = init_test_server().await.unwrap();
    let client = Client::connect(&server).await.unwrap();

    let node_id = NodeId::from_str("0x00069fc6fd02afeca110b9c32a21fb8ad899ee0a")?;

    let session = client.init_session(node_id).await.unwrap();
    let _endpoints = client.register_endpoints(session, vec![]).await.unwrap();

    // Change session id to invalid.
    let mut session = session.vec();
    session[0] = session[0] + 1;
    let session = SessionId::try_from(session)?;

    let response = client.find_node(session, node_id).await.unwrap();

    Ok(())
}
