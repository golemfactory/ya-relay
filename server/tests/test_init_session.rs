use ya_client_model::node_id::NodeId;
use ya_net_server::testing::key::generate;
use ya_net_server::testing::server::init_test_server;
use ya_net_server::testing::ClientBuilder;

#[serial_test::serial]
async fn test_query_self_node_info() -> anyhow::Result<()> {
    let server = init_test_server().await.unwrap();

    let client_key = generate();
    let node_id = NodeId::from(*client_key.public().address());
    let builder = ClientBuilder::from_server(&server, Some(client_key)).await;
    let client = builder.build().await.unwrap();

    let session = client.init_session().await.unwrap();
    let node_info = client.find_node(session.clone(), node_id).await.unwrap();

    // TODO: More checks, after everything will be implemented.
    assert_eq!(node_id, NodeId::from(&node_info.node_id[..]));
    Ok(())
}
