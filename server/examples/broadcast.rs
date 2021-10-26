use env_logger;
use futures::{SinkExt};

use ya_client_model::NodeId;
use ya_net_server::testing::server::{init_test_server, ServerWrapper};
use ya_net_server::testing::{Client, ClientBuilder};

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

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder().is_test(true).try_init()?;
    let wrapper = init_test_server().await.unwrap();
    let clients = start_clients(&wrapper, 2).await;
    let lost_node_id = {
        let dropped_client = start_clients(&wrapper, 1).await;
        // Uncomment to cleanly close connection
        // dropped_client[0].server_session().await?.close().await;
        dropped_client[0].node_id().await
    };

    let node_id = clients[0].node_id().await;
    let session = clients[0].server_session().await?;

    let ids = session.clone()
        .neighbours(5)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .map(|node| NodeId::from(node.node_id.as_ref()))
        .collect::<Vec<NodeId>>();

    let node = session.find_node(lost_node_id).await?;
    log::info!("Lost node = {:?}", node);

    let mut tx1 = session.forward(node.slot).await?;

    tx1.send(vec![1u8]).await?;

    // let closing = clients[2].server_session().await?.close();
    session.broadcast(vec!(), 5).await?;
    // closing.await;

    // Node itself isn't returned in it's neighbourhood.
    assert_eq!(ids.contains(&node_id), false);

    // Node neighbourhood consists of all nodes beside requesting node.
    assert_eq!(ids.len(), 2);
    Ok(())
}
