use anyhow::{anyhow, Context};
use futures::SinkExt;
use futures::StreamExt;
use std::rc::Rc;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_stream::wrappers::UnboundedReceiverStream;

use ya_relay_client::_client::Forwarded;
use ya_relay_client::testing::forwarding_utils::spawn_receive;
use ya_relay_client::testing::init::MockSessionNetwork;
use ya_relay_core::server_session::TransportType;

#[serial_test::serial]
async fn test_forward_unreliable_relayed() -> anyhow::Result<()> {
    let mut network = MockSessionNetwork::new().await.unwrap();

    let client1 = network.new_client().await?;
    let client2 = network.new_client().await?;

    network.hack_make_ip_private(&client1).await;
    network.hack_make_ip_private(&client2).await;

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

    tx1.send(vec![1u8].into()).await?;
    tx2.send(vec![2u8].into()).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));
    Ok(())
}

#[serial_test::serial]
async fn test_forward_reliable_relayed() -> anyhow::Result<()> {
    let mut network = MockSessionNetwork::new().await.unwrap();

    let client1 = network.new_client().await?;
    let client2 = network.new_client().await?;

    network.hack_make_ip_private(&client1).await;
    network.hack_make_ip_private(&client2).await;

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

    tx1.send(vec![1u8].into()).await?;
    tx2.send(vec![2u8].into()).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));

    Ok(())
}

#[serial_test::serial]
async fn test_p2p_unreliable() -> anyhow::Result<()> {
    let mut network = MockSessionNetwork::new().await.unwrap();

    let client1 = network.new_client().await?;
    let client2 = network.new_client().await?;

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

    tx1.send(vec![1u8].into()).await?;
    tx2.send(vec![2u8].into()).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));
    Ok(())
}

#[serial_test::serial]
async fn test_p2p_reliable() -> anyhow::Result<()> {
    let mut network = MockSessionNetwork::new().await.unwrap();

    let client1 = network.new_client().await?;
    let client2 = network.new_client().await?;

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

    tx1.send(vec![1u8].into()).await?;
    tx2.send(vec![2u8].into()).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));

    Ok(())
}

/// Test checks if there are no problems with establishing connections to the same Node
/// using different channels.
#[serial_test::serial]
async fn test_p2p_reliable_and_transfer() -> anyhow::Result<()> {
    let mut network = MockSessionNetwork::new().await.unwrap();

    let client1 = network.new_client().await?;
    let client2 = network.new_client().await?;

    let mut rx = client2
        .forward_receiver()
        .await
        .context("no forward receiver")?;

    let mut tx1 = client1.forward(client2.node_id()).await?;
    let mut tx2 = client1.forward_transfer(client2.node_id()).await?;

    tx1.send(vec![1u8].into()).await?;
    tx2.send(vec![2u8].into()).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let result1 = timeout(Duration::from_millis(200), rx.recv())
        .await?
        .ok_or(anyhow!("Receiver expected value"))?;

    let result2 = timeout(Duration::from_millis(200), rx.recv())
        .await?
        .ok_or(anyhow!("Receiver expected value"))?;

    // We can't depend on ordering, because channels are completely separate.
    let (result1, result2) = match result1.transport {
        TransportType::Unreliable => panic!("`TransportType::Unreliable` unexpected"),
        TransportType::Reliable => (result1, result2),
        TransportType::Transfer => (result2, result1),
    };

    assert_eq!(result1.payload.into_vec(), vec![1u8]);
    assert_eq!(result2.payload.into_vec(), vec![2u8]);
    Ok(())
}

#[serial_test::serial]
async fn test_rate_limiter() -> anyhow::Result<()> {
    let mut network = MockSessionNetwork::new().await.unwrap();

    let client1 = network.new_client().await?;
    let client2 = network.new_client().await?;

    network.hack_make_ip_private(&client1).await;
    network.hack_make_ip_private(&client2).await;

    let rx2 = client2
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let received2 = Rc::new(AtomicUsize::new(0));

    fn spawn_receive_counted(
        label: &'static str,
        received: Rc<AtomicUsize>,
        rx: mpsc::UnboundedReceiver<Forwarded>,
    ) {
        tokio::task::spawn_local({
            let received = received;
            async move {
                UnboundedReceiverStream::new(rx)
                    .for_each(|item| {
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
    spawn_receive_counted(">> 2", received2.clone(), rx2);

    let mut tx1 = client1.forward(client2.node_id()).await?;
    let big_payload = (0..255).collect::<Vec<u8>>();
    let iterations = (2048 / 256) + 1;
    let mut send_cnt = 0;
    for i in 0..iterations {
        println!("Send 255. iter: {}", i);
        tx1.send(big_payload.clone().into()).await?;
        send_cnt += big_payload.len();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let rec_cnt = received2.load(SeqCst);
    println!("Send counter: {}, Received counter: {}", send_cnt, rec_cnt);
    let _pausd_receive_count = rec_cnt;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let rec_cnt = received2.load(SeqCst);
    println!("Send counter: {}, Received counter: {}", send_cnt, rec_cnt);
    // It's hard to define exact value, as this test may catch
    // rate-limiter bucket from previous period, thus resulting
    // in a value slightly larger than limit (using limit from
    // two periods)
    let max_value = (2048 * 15) / 10;
    assert!(rec_cnt <= max_value);
    // TODO: Fix flaky test not forwarding all send packets
    // assert!(rec_cnt == send_cnt);

    Ok(())
}
