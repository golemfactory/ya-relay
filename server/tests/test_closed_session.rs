use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::time::Duration;
use test_case::test_case;
use ya_relay_client::{ClientBuilder, FailFast};
use ya_relay_core::crypto::{CryptoProvider, FallbackCryptoProvider};
use ya_relay_core::key::generate;
use ya_relay_server::testing::server::init_test_server;

enum Node {
    WithAlias,
    WithoutAlias,
}

#[test_case(Node::WithAlias ; "shutdowning node with alias")]
#[test_case(Node::WithoutAlias; "shutdowning node without alias")]
#[serial_test::serial]
async fn test_closed_session(node_to_shutdown: Node) -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let mut crypto = FallbackCryptoProvider::default();
    crypto.add(generate());

    let alias = crypto.aliases().await.unwrap()[0];

    let mut client = ClientBuilder::from_url(wrapper.url())
        .connect(FailFast::Yes)
        .build()
        .await?;
    let mut client_w_alias = ClientBuilder::from_url(wrapper.url())
        .connect(FailFast::Yes)
        .crypto(crypto)
        .build()
        .await?;

    let _ = client.forward_unreliable(alias).await.unwrap();
    let _ = client_w_alias
        .forward_unreliable(client.node_id())
        .await
        .unwrap();

    let node_1 = client.node_id();
    let node_2 = client_w_alias.node_id();

    let nodes = tuples_vec_to_map(client.connected_nodes().await);
    let expected_nodes = HashMap::from([
        (node_2.clone(), HashSet::new()),
        (alias.clone(), HashSet::from([node_2.clone()])),
    ]);
    assert_eq!(nodes, expected_nodes, "1 sees 2's default id and alias");

    let nodes = tuples_vec_to_map(client_w_alias.connected_nodes().await);
    let expected_nodes = HashMap::from([(node_1.clone(), HashSet::new())]);
    assert_eq!(nodes, expected_nodes, "2 sees only 1's default id");

    // Closing session
    match node_to_shutdown {
        Node::WithAlias => client_w_alias.shutdown().await.unwrap(),
        Node::WithoutAlias => client.shutdown().await.unwrap(),
    }

    // Waiting
    tokio::time::sleep(Duration::from_millis(100)).await;

    let nodes = tuples_vec_to_map(client.connected_nodes().await);
    let expected_nodes = HashMap::new();
    assert_eq!(
        nodes, expected_nodes,
        "1 does not see both 2's default id and alias"
    );

    let nodes = tuples_vec_to_map(client_w_alias.connected_nodes().await);
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
