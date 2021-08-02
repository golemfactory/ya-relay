use ya_client_model::node_id::NodeId;
use ya_net_server::testing::key::load_or_generate;
use ya_net_server::testing::server::init_test_server;
use ya_net_server::testing::Client;

#[serial_test::serial]
async fn test_query_self_node_info() -> anyhow::Result<()> {
    let server = init_test_server().await.unwrap();

    let client_key = load_or_generate("client.key.json", None);
    let node_id = NodeId::from(*client_key.public().address());
    let client = Client::connect(&server, client_key).await.unwrap();

    let session = client.init_session().await.unwrap();
    let node_info = client.find_node(session.clone(), node_id).await.unwrap();

    // TODO: More checks, after everything will be implemented.
    assert_eq!(node_id, NodeId::from(&node_info.node_id[..]));
    Ok(())
}
