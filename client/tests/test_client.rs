use std::time::Duration;
use ya_relay_client::{ClientBuilder, FailFast};
use ya_relay_core::utils::to_udp_url;
use ya_relay_server::testing::server::init_test_server;

/// Client should be able to use the same port after it was shutdown.
/// If it doesn't, it means that socket wasn't dropped correctly.
#[serial_test::serial]
async fn test_clean_shutdown() -> anyhow::Result<()> {
    let wrapper = init_test_server().await?;

    let mut client = ClientBuilder::from_url(wrapper.url())
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    println!("Shutting down Client");

    let addr2 = client.bind_addr().await?;
    let crypto = client.config.crypto.clone();

    client.shutdown().await.unwrap();

    println!("Waiting for session cleanup.");

    // Wait expiration timeout + ping timeout + 1s margin
    tokio::time::sleep(Duration::from_secs(6)).await;

    println!("Starting client");

    // Start client on the same port as previously.
    let mut _client = ClientBuilder::from_url(wrapper.url())
        .listen(to_udp_url(addr2).unwrap())
        .crypto(crypto)
        .connect(FailFast::Yes)
        .expire_session_after(Duration::from_secs(2))
        .build()
        .await?;

    Ok(())
}
