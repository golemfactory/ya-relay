mod helpers;

use anyhow::Context;
use futures::SinkExt;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use ya_relay_client::testing::forwarding_utils::spawn_receive;
use ya_relay_client::ClientBuilder;
use ya_relay_core::crypto::{CryptoProvider, FallbackCryptoProvider};
use ya_relay_core::key::generate;
use ya_relay_server::testing::server::init_test_server;

use helpers::hack_make_ip_private;

#[serial_test::serial]
async fn test_find_node_by_alias() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let mut crypto = FallbackCryptoProvider::default();
    crypto.add(generate());

    let alias = crypto.aliases().await.unwrap()[0];

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .crypto(crypto)
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

    let mut tx1 = client1.forward_unreliable(alias).await.unwrap();
    let mut tx2 = client2.forward_unreliable(client1.node_id()).await.unwrap();

    tx1.send(vec![1u8]).await?;
    tx2.send(vec![2u8]).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(received1.load(SeqCst));
    assert!(received2.load(SeqCst));
    Ok(())
}

#[serial_test::serial]
async fn test_find_node_by_alias_private_ip() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let mut crypto = FallbackCryptoProvider::default();
    crypto.add(generate());

    let alias = crypto.aliases().await.unwrap()[0];

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .crypto(crypto)
        .build()
        .await?;

    hack_make_ip_private(&wrapper, &client1).await;
    hack_make_ip_private(&wrapper, &client2).await;

    let _rx1 = client1
        .forward_receiver()
        .await
        .context("no forward receiver")?;
    let rx2 = client2
        .forward_receiver()
        .await
        .context("no forward receiver")?;

    let received2 = Rc::new(AtomicBool::new(false));

    println!("Setting up");

    spawn_receive(">> 2", received2.clone(), rx2);

    println!("Forwarding: unreliable");

    let mut tx1 = client1.forward_unreliable(alias).await.unwrap();
    tx1.send(vec![1u8]).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(received2.load(SeqCst));

    let mut tx1 = client1.forward_unreliable(client2.node_id()).await.unwrap();
    tx1.send(vec![1u8]).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(received2.load(SeqCst));

    Ok(())
}
