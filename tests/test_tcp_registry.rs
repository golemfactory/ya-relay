mod common;

use common::init::MockSessionNetwork;
use ya_relay_core::NodeId;
use ya_relay_client::test_utils::*;

/// `VirtNode` code assumes that channels are residing under index, that can be returned
/// by `ChannelDesc::index` function.
#[actix_rt::test]
async fn test_virt_node_creation_assumption() {
    let mut network = MockSessionNetwork::new().await.unwrap();
    let layer = network.new_layer().await.unwrap().layer;

    let virt = VirtNode::new(NodeId::default(), layer);
    for i in 0..4 {
        assert_eq!(virt.channels[i].channel.index(), i);
    }
}
