mod helpers;

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::time::Duration;
use ya_relay_client::ClientBuilder;
use ya_relay_core::crypto::{CryptoProvider, FallbackCryptoProvider};
use ya_relay_core::key::generate;
use ya_relay_server::testing::server::init_test_server;

#[serial_test::serial]
async fn test_closing_session() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let mut crypto = FallbackCryptoProvider::default();
    crypto.add(generate());

    let alias_2 = crypto.aliases().await.unwrap()[0];

    let mut client_1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    let client_2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .crypto(crypto)
        .build()
        .await?;

    let _ = client_1.forward_unreliable(alias_2).await.unwrap();
    let _ = client_2
        .forward_unreliable(client_1.node_id())
        .await
        .unwrap();

    let node_1 = client_1.node_id();
    let node_2 = client_2.node_id();

    let nodes = tuples_vec_to_map(client_1.connected_nodes().await);
    let expected_nodes = HashMap::from([
        (node_2.clone(), HashSet::new()),
        (alias_2.clone(), HashSet::from([node_2.clone()])),
    ]);
    assert_eq!(nodes, expected_nodes, "1 sees 2's default id and alias");

    let nodes = tuples_vec_to_map(client_2.connected_nodes().await);
    let expected_nodes = HashMap::from([(node_1.clone(), HashSet::new())]);
    assert_eq!(nodes, expected_nodes, "2 sees only 1's default id");

    // Closing session
    client_1.shutdown().await.unwrap();

    // Waiting
    tokio::time::sleep(Duration::from_millis(100)).await;

    let nodes = tuples_vec_to_map(client_1.connected_nodes().await);
    let expected_nodes = HashMap::new();
    assert_eq!(
        nodes, expected_nodes,
        "1 does not see both 2's default id and alias"
    );

    let nodes = tuples_vec_to_map(client_2.connected_nodes().await);
    let expected_nodes = HashMap::new();
    assert_eq!(nodes, expected_nodes, "2 does not see 1's default id");
    Ok(())
}

fn tuples_vec_to_map<NODE: Eq + PartialEq + Hash + Clone>(
    v: Vec<(NODE, Option<NODE>)>,
) -> HashMap<NODE, HashSet<NODE>> {
    v.into_iter().fold(
        HashMap::new(),
        |mut acc: HashMap<NODE, HashSet<NODE>>, (a, b): (NODE, Option<NODE>)| {
            if !acc.contains_key(&a) {
                acc.insert(a.clone(), HashSet::new());
            }
            if let Some(b) = b {
                acc.get_mut(&a).unwrap().insert(b);
            }
            acc
        },
    )
}
