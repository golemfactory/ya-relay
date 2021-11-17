use std::rc::Rc;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use ya_relay_client::ClientBuilder;
use ya_relay_client::client::Forwarded;
use ya_relay_server::testing::server::init_test_server;

#[serial_test::serial]
async fn test_two_way_packet_forward() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;

    let rx1 = client1
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let rx2 = client2
        .forward_receiver()
        .await
        .context("no forward receiver")?;

    let session1 = client1.server_session().await?;
    let session2 = client2.server_session().await?;

    let node1 = session2.find_node(client1.node_id().await).await?;
    let node2 = session1.find_node(client2.node_id().await).await?;

    println!("Node 1 slot: {}", node1.slot);
    println!("Node 2 slot: {}", node2.slot);

    let received1 = Rc::new(AtomicBool::new(false));
    let received2 = Rc::new(AtomicBool::new(false));

    fn spawn_receive<T: std::fmt::Debug + 'static>(
        label: &'static str,
        received: Rc<AtomicBool>,
        rx: mpsc::UnboundedReceiver<T>,
    ) {
        tokio::task::spawn_local({
            let received = received.clone();
            async move {
                rx.for_each(|item| {
                    let received = received.clone();
                    async move {
                        println!("{} received {:?}", label, item);
                        received.clone().store(true, SeqCst)
                    }
                })
                .await;
            }
        });
    }

    println!("Setting up");

    spawn_receive(">> 1", received1.clone(), rx1);
    spawn_receive(">> 2", received2.clone(), rx2);

    println!("Forwarding: unreliable");

    let mut tx1 = session1.forward_unreliable(node2.slot).await?;
    let mut tx2 = session2.forward_unreliable(node1.slot).await?;

    tx1.send(vec![1u8]).await?;
    tx2.send(vec![2u8]).await?;

    tokio::time::delay_for(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));

    received1.store(false, SeqCst);
    received2.store(false, SeqCst);

    println!("Forwarding: reliable");

    let mut tx1 = session1.forward(node2.slot).await?;
    let mut tx2 = session2.forward(node1.slot).await?;

    tx1.send(vec![1u8]).await?;
    tx2.send(vec![2u8]).await?;

    tokio::time::delay_for(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));

    Ok(())
}

#[serial_test::serial]
async fn test_rate_limiter() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;

    let rx2 = client2
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let session1 = client1.server_session().await?;
    let node2 = session1.find_node(client2.node_id().await).await?;
    println!("Node 2 slot: {}", node2.slot);
    let received2 = Rc::new(AtomicUsize::new(0));

    fn spawn_receive(
        label: &'static str,
        received: Rc<AtomicUsize>,
        rx: mpsc::UnboundedReceiver<Forwarded>,
    ) {
        tokio::task::spawn_local({
            let received = received.clone();
            async move {
                rx.for_each(|item| {
                    let received = received.clone();
                    async move {
                        let last_val = received.clone().fetch_add(item.payload.len(), SeqCst);
                        println!("{} received {:?} last_val: {}", label, item, last_val + 1);
                    }
                })
                .await;
            }
        });
    }
    spawn_receive(">> 2", received2.clone(), rx2);

    let mut tx1 = session1.forward(node2.slot).await?;
    let big_payload = (0..255).collect::<Vec<u8>>();
    for _ in 0..10 {
        println!("Send 255");
        tx1.send(big_payload.clone()).await?;
    }
    tokio::time::delay_for(Duration::from_millis(100)).await;
    let rec_cnt = received2.load(SeqCst);
    println!("Received counter: {}", rec_cnt);
    assert!(rec_cnt <= 2048);

    Ok(())
}
