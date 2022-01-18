use std::time::Duration;
use url::Url;

use ya_relay_client::testing::forwarding_utils::{check_forwarding, spawn_receive_for_client};
use ya_relay_client::ClientBuilder;
use ya_relay_server::testing::server::init_test_server;

#[serial_test::serial]
async fn test_restarting_p2p_session() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;
    let mut client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker1 = spawn_receive_for_client(&client1, "Client1").await?;
    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let _keep1 = check_forwarding(&client1, &client2, marker2.clone())
        .await
        .unwrap();

    let _keep2 = check_forwarding(&client2, &client1, marker1.clone())
        .await
        .unwrap();

    println!("Shutting down Client2");

    let addr2 = client2.bind_addr().await?;
    let crypto = client2.config.crypto.clone();

    client2.shutdown().await.unwrap();
    drop(_keep2);

    println!("Waiting for session cleanup.");

    tokio::time::delay_for(Duration::from_secs(5)).await;

    println!("Starting Client2");

    // Start client on the same port as previously.
    let client2 = ClientBuilder::from_url(wrapper.url())
        .listen(Url::parse(&format!("udp://{}:{}", addr2.ip(), addr2.port())).unwrap())
        .crypto(crypto)
        .connect()
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let _keep = check_forwarding(&client1, &client2, marker2.clone())
        .await
        .unwrap();

    let _keep = check_forwarding(&client2, &client1, marker1.clone())
        .await
        .unwrap();

    Ok(())
}
