use std::convert::{TryFrom, TryInto};

use ya_relay_client::testing::Session;
use ya_relay_client::ClientBuilder;
use ya_relay_core::server_session::SessionId;
use ya_relay_proto::proto;
use ya_relay_server::testing::server::init_test_server;

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

    let session = client.sessions.server_session().await.unwrap();
    let node_info = session.find_node(node_id).await.unwrap();

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

#[serial_test::serial]
async fn test_request_with_invalid_session() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let client = ClientBuilder::from_url(wrapper.server.inner.url.clone())
        .connect()
        .build()
        .await
        .unwrap();

    let node_id = client.node_id();
    let session = client.sessions.server_session().await?;
    let session_id = session.id;

    // Change session id to invalid.
    let mut session_id = session_id.to_vec();
    session_id[0] += 1;
    let session_id = SessionId::try_from(session_id)?;

    let session = Session::new(session.remote, session_id, client.sessions.out_stream()?);

    let result = session.find_node(node_id).await;
    assert!(result.is_err());

    Ok(())
}

#[serial_test::serial]
async fn test_query_other_node_info() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let client1 = ClientBuilder::from_url(wrapper.server.inner.url.clone())
        .connect()
        .build()
        .await
        .unwrap();
    let client2 = ClientBuilder::from_url(wrapper.server.inner.url.clone())
        .connect()
        .build()
        .await
        .unwrap();

    let node1_id = client1.node_id();
    let node2_id = client2.node_id();

    let session1 = client1.sessions.server_session().await?;
    let session2 = client2.sessions.server_session().await?;

    let node2_info = session1.find_node(node2_id).await.unwrap();
    let node1_info = session2.find_node(node1_id).await.unwrap();

    assert_eq!(
        node1_id,
        (&node1_info.identities[0].node_id).try_into().unwrap()
    );
    assert_eq!(
        node2_id,
        (&node2_info.identities[0].node_id).try_into().unwrap()
    );
    Ok(())
}
