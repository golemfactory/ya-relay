mod common;

use ya_relay_client::testing::init::MockSessionNetwork;
use ya_relay_client::testing::private::*;
use ya_relay_core::NodeId;
use ya_relay_server::testing::server::init_test_server;

/// `VirtNode` code assumes that channels are residing under index, that can be returned
/// by `ChannelDesc::index` function.
#[actix_rt::test]
async fn test_virt_node_creation_assumption() {
    let server = init_test_server().await.unwrap();
    let mut network = MockSessionNetwork::new(server).unwrap();
    let layer = network.new_layer().await.unwrap().layer;

    let virt = VirtNode::new(NodeId::default(), layer);
    for i in 0..4 {
        assert_eq!(virt.channels[i].channel.index(), i);
    }
}
