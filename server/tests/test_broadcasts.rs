use itertools::Itertools;

use ya_client_model::NodeId;
use ya_relay_server::testing::server::{init_test_server, ServerWrapper};
use ya_relay_server::testing::{Client, ClientBuilder};

async fn start_clients(wrapper: &ServerWrapper, count: u32) -> Vec<Client> {
    let mut clients = vec![];
    for _ in 0..count {
        clients.push(
            ClientBuilder::from_server(&wrapper.server)
                .connect()
                .build()
                .await
                .unwrap(),
        )
    }
    clients
}

#[serial_test::serial]
async fn test_neighbourhood() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let clients = start_clients(&wrapper, 13).await;

    let node_id = clients[0].node_id().await;
    let session = clients[0].server_session().await?;

    let ids = session
        .neighbours(5)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .map(|node| NodeId::from(node.node_id.as_ref()))
        .collect::<Vec<NodeId>>();

    // Node itself isn't returned in it's neighbourhood.
    assert_eq!(ids.contains(&node_id), false);

    // No duplicated nodes in neighbourhood.
    assert_eq!(ids.len(), 5);
    assert_eq!(ids.len(), ids.clone().into_iter().unique().count());

    // If no new nodes appeared or disappeared, server should return the same neighbourhood.
    let ids2 = session
        .neighbours(5)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .map(|node| NodeId::from(node.node_id.as_ref()))
        .collect::<Vec<NodeId>>();

    assert_eq!(ids, ids2);

    // When we take bigger neighbourhood it should contain smaller neighbouthood.
    let ids3 = session
        .neighbours(8)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .map(|node| NodeId::from(node.node_id.as_ref()))
        .collect::<Vec<NodeId>>();

    assert!(ids2.iter().all(|item| ids3.contains(item)));
    Ok(())
}

#[serial_test::serial]
async fn test_neighbourhood_too_big_neighbourhood_request() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let clients = start_clients(&wrapper, 3).await;

    let node_id = clients[0].node_id().await;
    let session = clients[0].server_session().await?;

    let ids = session
        .neighbours(5)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .map(|node| NodeId::from(node.node_id.as_ref()))
        .collect::<Vec<NodeId>>();

    // Node itself isn't returned in it's neighbourhood.
    assert_eq!(ids.contains(&node_id), false);

    // Node neighbourhood consists of all nodes beside requesting node.
    assert_eq!(ids.len(), 2);
    Ok(())
}
