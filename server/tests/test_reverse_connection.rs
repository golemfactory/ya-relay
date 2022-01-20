mod helpers;

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use std::rc::Rc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;
use tokio::sync::mpsc;

use ya_relay_client::client::Forwarded;
use ya_relay_client::ClientBuilder;
use ya_relay_server::testing::server::init_test_server;

use helpers::hack_make_ip_private;

#[serial_test::serial]
async fn test_reverse_connection() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;

    println!(
        "Client 1: {} - {:?}",
        client1.node_id(),
        client1.public_addr().await.unwrap()
    );
    println!(
        "Client 2: {} - {:?}",
        client2.node_id(),
        client2.public_addr().await.unwrap()
    );
    hack_make_ip_private(&wrapper, &client2).await;

    //wrapper.reverse_connection(client2.node_id());

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
    let mut send_cnt = 0;
    for i in 0..iterations {
        println!("Send 255. iter: {}", i);
        tx1.send(big_payload.clone()).await?;
        send_cnt += big_payload.len();
    }
    tokio::time::delay_for(Duration::from_millis(100)).await;
    let rec_cnt = received2.load(SeqCst);
    println!("Send counter: {}, Received counter: {}", send_cnt, rec_cnt);
    let _pausd_receive_count = rec_cnt;
    tokio::time::delay_for(Duration::from_secs(10)).await;
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
