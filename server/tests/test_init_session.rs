use std::convert::TryFrom;

use ya_client_model::NodeId;
use ya_net_server::testing::key::generate;
use ya_net_server::testing::server::init_test_server;
use ya_net_server::testing::ClientBuilder;
use ya_net_server::SessionId;
use ya_relay_proto::proto;

#[serial_test::serial]
async fn test_query_self_node_info() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let client = ClientBuilder::from_server(&wrapper.server)
        .with_secret(generate())
        .build()
        .await
        .unwrap();

    let node_id = client.id().await;
    let endpoints = vec![proto::Endpoint {
        protocol: proto::Protocol::Udp as i32,
        address: "127.0.0.1".to_string(),
        port: client.inner.read().await.socket.local_addr()?.port() as u32,
    }];

    let session = client.init_session().await.unwrap();
    let result_endpoints = client.register_endpoints(session, vec![]).await.unwrap();
    let node_info = client.find_node(session, node_id).await.unwrap();

    // TODO: More checks, after everything will be implemented.
    assert_eq!(node_id, NodeId::from(&node_info.node_id[..]));
    assert_eq!(node_info.random, false);
    assert_ne!(node_info.slot, u32::max_value());
    assert_eq!(node_info.endpoints.len(), 1);
    assert_eq!(node_info.endpoints[0], endpoints[0]);

    // Check server response.
    assert_eq!(result_endpoints.len(), 1);
    assert_eq!(result_endpoints[0], endpoints[0]);
    Ok(())
}

#[serial_test::serial]
async fn test_request_with_invalid_session() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let client = ClientBuilder::from_server(&wrapper.server)
        .with_secret(generate())
        .build()
        .await
        .unwrap();

    let node_id = client.id().await;

    let session = client.init_session().await.unwrap();
    let _endpoints = client.register_endpoints(session, vec![]).await.unwrap();

    // Change session id to invalid.
    let mut session = session.vec();
    session[0] = session[0] + 1;
    let session = SessionId::try_from(session)?;

    let result = client.find_node(session, node_id).await;
    assert!(result.is_err());

    Ok(())
}

#[serial_test::serial]
async fn test_query_other_node_info() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let client1 = ClientBuilder::from_server(&wrapper.server)
        .with_secret(generate())
        .build()
        .await
        .unwrap();
    let client2 = ClientBuilder::from_server(&wrapper.server)
        .with_secret(generate())
        .build()
        .await
        .unwrap();

    let node1_id = client1.id().await;
    let node2_id = client2.id().await;

    let session1 = client1.init_session().await.unwrap();
    let _endpoints = client1.register_endpoints(session1, vec![]).await.unwrap();

    let session2 = client2.init_session().await.unwrap();
    let _endpoints = client2.register_endpoints(session2, vec![]).await.unwrap();

    let node2_info = client1.find_node(session1, node2_id).await.unwrap();
    let node1_info = client2.find_node(session2, node1_id).await.unwrap();

    assert_eq!(node1_id, NodeId::from(&node1_info.node_id[..]));
    assert_eq!(node2_id, NodeId::from(&node2_info.node_id[..]));
    Ok(())
}
