use std::convert::TryInto;

use ya_relay_client::ClientBuilder;
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
