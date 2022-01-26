mod helpers;

use anyhow::Context;
use futures::SinkExt;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use ya_relay_client::ClientBuilder;
use ya_relay_server::testing::server::init_test_server;

use helpers::{hack_make_ip_private, spawn_receive};

#[serial_test::serial]
async fn test_reverse_connection() -> anyhow::Result<()> {
    // Test if reverse connection is setup when client1 is public and client2 is private
    let wrapper = init_test_server().await?;

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    println!(
        "Client 1: {} - {:?}",
        client1.node_id(),
        client1.public_addr().await.unwrap()
    );

    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    println!(
        "Client 2: {} - {:?}",
        client2.node_id(),
        client2.public_addr().await.unwrap()
    );

    hack_make_ip_private(&wrapper, &client2).await;
    println!("Setup completed");

    // pub => priv
    println!("Sending forward message from pub -> priv");
    let rx2 = client2
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let received2 = Rc::new(AtomicBool::new(false));
    spawn_receive(">> 2", received2.clone(), rx2);

    println!("Forwarding: unreliable");
    let mut tx1 = client1.forward_unreliable(client2.node_id()).await.unwrap();
    println!("forwarder setup");

    tx1.send(vec![1u8]).await?;
    println!("message send");

    tokio::time::delay_for(Duration::from_millis(100)).await;
    assert!(received2.load(SeqCst));
    println!("message confirmed");

    // priv => pub
    println!("Sending forward message from priv -> pub");
    let rx1 = client1
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let received1 = Rc::new(AtomicBool::new(false));
    spawn_receive(">> 1", received1.clone(), rx1);

    println!("Forwarding: unreliable");
    let mut tx2 = client2.forward_unreliable(client1.node_id()).await.unwrap();
    println!("forwarder setup");

    tx2.send(vec![2u8]).await?;
    println!("message send");

    tokio::time::delay_for(Duration::from_millis(100)).await;
    assert!(received1.load(SeqCst));
    println!("message confirmed");
    Ok(())
}

#[serial_test::serial]
async fn test_reverse_connection_fail_both_private() -> anyhow::Result<()> {
    // Test if server forwarded connections are used when both clients are private
    let wrapper = init_test_server().await?;

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    println!(
        "Client 1: {} - {:?}",
        client1.node_id(),
        client1.public_addr().await.unwrap()
    );

    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    println!(
        "Client 2: {} - {:?}",
        client2.node_id(),
        client2.public_addr().await.unwrap()
    );

    hack_make_ip_private(&wrapper, &client1).await;
    hack_make_ip_private(&wrapper, &client2).await;

    println!("Setup completed");

    // pub => priv
    println!("Sending forward message from client1 -> client2");
    let rx2 = client2
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let received2 = Rc::new(AtomicBool::new(false));
    spawn_receive(">> 2", received2.clone(), rx2);

    println!("Forwarding: unreliable");
    let mut tx1 = client1.forward_unreliable(client2.node_id()).await.unwrap();
    println!("forwarder setup");

    tx1.send(vec![1u8]).await?;
    println!("message send");

    tokio::time::delay_for(Duration::from_millis(100)).await;
    assert!(received2.load(SeqCst));
    println!("message confirmed");

    // priv => pub
    println!("Sending forward message from client2 -> client1");
    let rx1 = client1
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let received1 = Rc::new(AtomicBool::new(false));
    spawn_receive(">> 1", received1.clone(), rx1);

    println!("Forwarding: unreliable");
    let mut tx2 = client2.forward_unreliable(client1.node_id()).await.unwrap();
    println!("forwarder setup");

    tx2.send(vec![2u8]).await?;
    println!("message send");
    tokio::time::delay_for(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    println!("message confirmed");

    Ok(())
}
