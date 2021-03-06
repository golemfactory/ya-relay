use std::net::UdpSocket;
use ya_relay_client::testing::forwarding_utils::{check_broadcast, spawn_receive_for_client};
use ya_relay_client::ClientBuilder;
use ya_relay_server::testing::server::init_test_server;

/// Server should not shutdown when receives junks (single, garbage bytes).
/// Testing if server does not shutdown when receives junks.
#[serial_test::serial]
async fn test_server_junks_received() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;

    let _bytes = socket
        .send_to(&[0; 1], wrapper.url().socket_addrs(|| None).unwrap()[0])
        .unwrap();

    let client1 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;
    let client2 = ClientBuilder::from_url(wrapper.url())
        .connect()
        .build()
        .await?;

    let marker1 = spawn_receive_for_client(&client1, "Client1").await?;
    let marker2 = spawn_receive_for_client(&client2, "Client2").await?;

    let _keep1 = check_broadcast(&client1, &client2, marker2.clone(), 2)
        .await
        .unwrap();

    let _keep2 = check_broadcast(&client2, &client1, marker1.clone(), 2)
        .await
        .unwrap();

    Ok(())
}
