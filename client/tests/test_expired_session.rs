#![cfg(feature = "mock")]
use std::time::Duration;

use ya_relay_client::testing::forwarding_utils::{
    check_broadcast, check_forwarding, spawn_receive_for_client, Mode,
};
use ya_relay_client::{ClientBuilder, FailFast};
use ya_relay_core::crypto::FallbackCryptoProvider;
use ya_relay_core::utils::to_udp_url;
use ya_relay_server::testing::server::{
    init_test_server, init_test_server_with_config, test_default_config,
};

#[serial_test::serial]
async fn test_restarting_p2p_session_tcp() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let crypto1 = FallbackCryptoProvider::default();
    let crypto2 = FallbackCryptoProvider::default();

    let client1 = ClientBuilder::from_url(wrapper.url())
        .crypto(crypto1.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;
    let mut client2 = ClientBuilder::from_url(wrapper.url())
        .crypto(crypto2.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker1 = spawn_receive_for_client(&client1, "Client1").await?;
    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let _keep1 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Reliable)
        .await
        .unwrap();

    let _keep2 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Reliable)
        .await
        .unwrap();

    println!("Shutting down Client2");

    let addr2 = client2.bind_addr().await?;

    client2.shutdown().await.unwrap();
    drop(_keep2);

    println!("Waiting for session cleanup.");

    // Wait expiration timeout + ping timeout + 1s margin
    tokio::time::sleep(Duration::from_secs(6)).await;

    println!("Starting Client2");

    // Start client on the same port as previously.
    let mut client2 = ClientBuilder::from_url(wrapper.url())
        .listen(to_udp_url(addr2).unwrap())
        .crypto(crypto2.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let _keep1 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Reliable)
        .await
        .unwrap();

    let _keep2 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Reliable)
        .await
        .unwrap();

    println!("Shutting down Client2");

    let addr2 = client2.bind_addr().await?;
    let crypto = crypto2.clone();

    client2.shutdown().await.unwrap();
    drop(_keep2);

    println!("Waiting for session cleanup.");

    // Wait expiration timeout + ping timeout + 1s margin
    tokio::time::sleep(Duration::from_secs(6)).await;

    println!("Starting Client2");

    // Start client on the same port as previously.
    let client2 = ClientBuilder::from_url(wrapper.url())
        .listen(to_udp_url(addr2).unwrap())
        .crypto(crypto)
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let _keep2 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Reliable)
        .await
        .unwrap();
    let _keep1 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Reliable)
        .await
        .unwrap();

    Ok(())
}

#[serial_test::serial]
async fn test_restarting_p2p_session_unreliable() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let crypto1 = FallbackCryptoProvider::default();
    let crypto2 = FallbackCryptoProvider::default();

    let client1 = ClientBuilder::from_url(wrapper.url())
        .crypto(crypto1.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;
    let mut client2 = ClientBuilder::from_url(wrapper.url())
        .crypto(crypto2.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker1 = spawn_receive_for_client(&client1, "Client1").await?;
    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let _keep1 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Unreliable)
        .await
        .unwrap();

    let _keep2 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Unreliable)
        .await
        .unwrap();

    println!("Shutting down Client2");

    let addr2 = client2.bind_addr().await?;
    let crypto = crypto2.clone();

    client2.shutdown().await.unwrap();
    drop(_keep2);

    println!("Waiting for session cleanup.");

    // Wait expiration timeout + ping timeout + 1s margin
    tokio::time::sleep(Duration::from_secs(6)).await;

    println!("Starting Client2");

    // Start client on the same port as previously.
    let mut client2 = ClientBuilder::from_url(wrapper.url())
        .listen(to_udp_url(addr2).unwrap())
        .crypto(crypto.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let _keep1 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Unreliable)
        .await
        .unwrap();

    let _keep2 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Unreliable)
        .await
        .unwrap();

    // Disconnect Client2 again and this time connect from client1 to client2 first.
    println!("Shutting down Client2 for the second time");

    client2.shutdown().await.unwrap();
    drop(_keep1);

    println!("Waiting for session cleanup.");

    // Wait expiration timeout + ping timeout + 1s margin
    tokio::time::sleep(Duration::from_secs(6)).await;

    println!("Starting Client2");

    // Start client on the same port as previously.
    let client2 = ClientBuilder::from_url(wrapper.url())
        .listen(to_udp_url(addr2).unwrap())
        .crypto(crypto.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let _keep2 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Unreliable)
        .await
        .unwrap();
    let _keep1 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Unreliable)
        .await
        .unwrap();

    Ok(())
}

#[serial_test::serial]
async fn test_restart_server() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker1 = spawn_receive_for_client(&client1, "Client1").await?;
    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let keep1 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Unreliable)
        .await
        .unwrap();

    let keep2 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Unreliable)
        .await
        .unwrap();

    // Abort server
    drop(wrapper);
    drop(keep1);
    drop(keep2);

    tokio::time::sleep(Duration::from_secs(6)).await;

    let _keep1 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Unreliable)
        .await
        .unwrap();

    let _keep2 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Unreliable)
        .await
        .unwrap();

    Ok(())
}

/// Client should get Node information about Nodes, that were previously in it's
/// neighborhood, but disappeared. If Relay doesn't store any information, than Node
/// should remove session and all information about peer.
#[serial_test::serial]
async fn test_restart_after_neighborhood_changed() -> anyhow::Result<()> {
    let mut config = test_default_config();
    config.session_cleaner_interval = Duration::from_secs(1);
    config.session_timeout = chrono::Duration::seconds(8);

    let wrapper = init_test_server_with_config(config).await?;

    let crypto2 = FallbackCryptoProvider::default();

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;
    let mut client2 = ClientBuilder::from_url(wrapper.url())
        .connect(FailFast::Yes)
        .crypto(crypto2.clone())
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker1 = spawn_receive_for_client(&client1, "Client1").await?;
    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    check_broadcast(&client1, &client2, marker2.clone(), 2)
        .await
        .unwrap();

    check_broadcast(&client2, &client1, marker1.clone(), 2)
        .await
        .unwrap();

    println!("Shutting down Client2");

    let addr2 = client2.bind_addr().await?;
    let crypto = crypto2.clone();

    client2.shutdown().await.unwrap();

    println!("Waiting for session cleanup on Client1.");

    // First broadcast will try to connect directly, after it will fail,
    // Node will send data through relay. Since Relay didn't timed out
    // Client2 yet, forwarding will seem to be successfully, although
    // packets won't reach destination.
    tokio::time::sleep(Duration::from_secs(5)).await;

    println!("Client1 will attempt to forward through relay.");
    client1.broadcast(vec![0x1], 2).await.unwrap();

    // Second broadcast will request for neighborhood that will change,
    // because relay already noticed at this point, that Client2 disconnected.
    // Client2 Node should be removed here from Client1 too, because `find_node`
    // call to relay will fail.
    tokio::time::sleep(Duration::from_secs(5)).await;
    println!("Client1 will request new neighborhood.");
    client1.broadcast(vec![0x1], 2).await.unwrap();

    tokio::time::sleep(Duration::from_secs(1)).await;

    println!("Starting Client2");

    // Start client on the same port as previously.
    let client2 = ClientBuilder::from_url(wrapper.url())
        .listen(to_udp_url(addr2).unwrap())
        .crypto(crypto.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    // Client1 should be able to connect to Client2 again.
    let _keep1 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Unreliable)
        .await
        .unwrap();

    let _keep2 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Unreliable)
        .await
        .unwrap();

    Ok(())
}

// In case of fast restart of Node, second Node will think, that session still exist.
// This causes problems, because one Node is sending packets and second is dropping them.
// If any Nodes gets Forward packet from unknown session, it should send disconnect message.
// This way second Node can establish new session.
#[serial_test::serial]
async fn test_fast_restart_unreliable() -> anyhow::Result<()> {
    let mut config = test_default_config();
    config.session_cleaner_interval = Duration::from_secs(1);
    config.session_timeout = chrono::Duration::seconds(15);

    let wrapper = init_test_server_with_config(config).await?;

    let crypto2 = FallbackCryptoProvider::default();

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(15))
        .build()
        .await?;
    let mut client2 = ClientBuilder::from_url(wrapper.url())
        .crypto(crypto2.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(15))
        .build()
        .await?;

    let marker1 = spawn_receive_for_client(&client1, "Client1").await?;
    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    check_broadcast(&client1, &client2, marker2.clone(), 2)
        .await
        .unwrap();

    check_broadcast(&client2, &client1, marker1.clone(), 2)
        .await
        .unwrap();

    println!("Shutting down Client2");

    let addr2 = client2.bind_addr().await?;
    let crypto = crypto2.clone();

    client2.shutdown().await.unwrap();

    println!("Waiting for session cleanup on Client1.");

    tokio::time::sleep(Duration::from_secs(2)).await;

    println!("Restarting Client2");

    // Start client on the same port as previously.
    let client2 = ClientBuilder::from_url(wrapper.url())
        .listen(to_udp_url(addr2).unwrap())
        .crypto(crypto.clone())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    println!("Client1 will send forward packet and will receive Disconnected message.");
    client1.broadcast(vec![0x1], 2).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Client1 should be able to connect to Client2 again.
    let _keep2 = check_forwarding(&client1, &client2, marker2.clone(), Mode::Unreliable)
        .await
        .unwrap();

    let _keep1 = check_forwarding(&client2, &client1, marker1.clone(), Mode::Unreliable)
        .await
        .unwrap();

    Ok(())
}
