use anyhow::Context;
use futures::{SinkExt, StreamExt};
use std::rc::Rc;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;
use tokio::sync::mpsc;

use ya_relay_client::client::Forwarded;
use ya_relay_client::testing::forwarding_utils::spawn_receive;
use ya_relay_client::{Client, ClientBuilder};
use ya_relay_server::testing::server::{init_test_server, ServerWrapper};

/// TODO: Should be moved to ServerWrapper, but we don't want to import Client in Server crate.
async fn hack_make_ip_private(wrapper: &ServerWrapper, client: &Client) {
    let mut state = wrapper.server.state.write().await;

    let mut info = state.nodes.get_by_node_id(client.node_id()).unwrap();
    state.nodes.remove_session(info.info.slot);

    // Server won't return any endpoints, so Client won't try to connect directly.
    info.info.endpoints = vec![];
    state.nodes.register(info)
}

#[serial_test::serial]
async fn test_forward_unreliable() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;

    hack_make_ip_private(&wrapper, &client1).await;
    hack_make_ip_private(&wrapper, &client2).await;

    let rx1 = client1
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let rx2 = client2
        .forward_receiver()
        .await
        .context("no forward receiver")?;

    let received1 = Rc::new(AtomicBool::new(false));
    let received2 = Rc::new(AtomicBool::new(false));

    println!("Setting up");

    spawn_receive(">> 1", received1.clone(), rx1);
    spawn_receive(">> 2", received2.clone(), rx2);

    println!("Forwarding: unreliable");

    let mut tx1 = client1.forward_unreliable(client2.node_id()).await.unwrap();
    let mut tx2 = client2.forward_unreliable(client1.node_id()).await.unwrap();

    tx1.send(vec![1u8]).await?;
    tx2.send(vec![2u8]).await?;

    tokio::time::delay_for(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));
    Ok(())
}

#[serial_test::serial]
async fn test_forward_reliable() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;

    hack_make_ip_private(&wrapper, &client1).await;
    hack_make_ip_private(&wrapper, &client2).await;

    let rx1 = client1
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let rx2 = client2
        .forward_receiver()
        .await
        .context("no forward receiver")?;

    let received1 = Rc::new(AtomicBool::new(false));
    let received2 = Rc::new(AtomicBool::new(false));

    println!("Setting up");

    spawn_receive(">> 1", received1.clone(), rx1);
    spawn_receive(">> 2", received2.clone(), rx2);

    println!("Forwarding: reliable");

    let mut tx1 = client1.forward(client2.node_id()).await.unwrap();
    let mut tx2 = client2.forward(client1.node_id()).await.unwrap();

    tx1.send(vec![1u8]).await?;
    tx2.send(vec![2u8]).await?;

    tokio::time::delay_for(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));

    Ok(())
}

#[serial_test::serial]
async fn test_p2p_unreliable() -> anyhow::Result<()> {
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

    let received1 = Rc::new(AtomicBool::new(false));
    let received2 = Rc::new(AtomicBool::new(false));

    println!("Setting up");

    spawn_receive(">> 1", received1.clone(), rx1);
    spawn_receive(">> 2", received2.clone(), rx2);

    println!("Forwarding: unreliable");

    let mut tx1 = client1.forward_unreliable(client2.node_id()).await.unwrap();
    let mut tx2 = client2.forward_unreliable(client1.node_id()).await.unwrap();

    tx1.send(vec![1u8]).await?;
    tx2.send(vec![2u8]).await?;

    tokio::time::delay_for(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));
    Ok(())
}

#[serial_test::serial]
async fn test_p2p_reliable() -> anyhow::Result<()> {
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

    let received1 = Rc::new(AtomicBool::new(false));
    let received2 = Rc::new(AtomicBool::new(false));

    println!("Setting up");

    spawn_receive(">> 1", received1.clone(), rx1);
    spawn_receive(">> 2", received2.clone(), rx2);

    println!("Forwarding: reliable");

    let mut tx1 = client1.forward(client2.node_id()).await?;
    let mut tx2 = client2.forward(client1.node_id()).await?;

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

    let mut tx1 = client1.forward(client2.node_id()).await?;
    let big_payload = (0..255).collect::<Vec<u8>>();
    let iterations = (2048 / 256) + 1;
    for i in 0..iterations {
        println!("Send 255. iter: {}", i);
        tx1.send(big_payload.clone()).await?;
    }
    tokio::time::delay_for(Duration::from_millis(100)).await;
    let rec_cnt = received2.load(SeqCst);
    println!("Received counter: {}", rec_cnt);
    // It's hard to define exact value, as this test may catch
    // rate-limiter bucket from previous period, thus resulting
    // in a value slightly larger than limit (using limit from
    // two periods)
    let max_value = (2048 * 15) / 10;
    assert!(rec_cnt <= max_value);

    Ok(())
}
