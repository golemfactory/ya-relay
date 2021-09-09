use std::iter::FromIterator;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use bytes::BytesMut;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};

use ya_net_server::testing::key::generate;
use ya_net_server::testing::server::init_test_server;
use ya_net_server::testing::ClientBuilder;

#[serial_test::serial]
async fn test_two_way_packet_forward() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let client1 = ClientBuilder::from_url(wrapper.url())
        .secret(generate())
        .connect()
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .secret(generate())
        .connect()
        .build()
        .await?;

    let session1 = client1.server_session().await?;
    let session2 = client2.server_session().await?;

    let node1_info = session2.find_node(client1.node_id().await).await?;
    let node2_info = session2.find_node(client2.node_id().await).await?;

    let (mut txs1, rxs1) = mpsc::channel(1);
    let (txr1, rxr1) = mpsc::channel(1);
    let (mut txs2, rxs2) = mpsc::channel(1);
    let (txr2, rxr2) = mpsc::channel(1);

    let node1_received = Rc::new(AtomicBool::new(false));
    let node2_received = Rc::new(AtomicBool::new(false));

    fn spawn_receive<T: 'static>(received: Rc<AtomicBool>, rx: mpsc::Receiver<T>) {
        tokio::task::spawn_local({
            let received = received.clone();
            async move {
                rx.for_each(|_| async { received.clone().store(true, SeqCst) })
                    .await;
            }
        });
    }

    spawn_receive(node1_received.clone(), rxr1);
    spawn_receive(node2_received.clone(), rxr2);

    println!("Node 1 slot: {}", node1_info.slot);
    println!("Node 2 slot: {}", node2_info.slot);

    session1.forward(node2_info.slot, rxs1, txr1).await?;
    session2.forward(node1_info.slot, rxs2, txr2).await?;

    txs1.send(BytesMut::from_iter([1u8].iter())).await?;
    txs2.send(BytesMut::from_iter([2u8].iter())).await?;

    tokio::time::delay_for(Duration::from_millis(100)).await;

    assert!(node1_received.load(SeqCst));
    assert!(node2_received.load(SeqCst));

    Ok(())
}
