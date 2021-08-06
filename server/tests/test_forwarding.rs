use ya_net_server::testing::key::generate;
use ya_net_server::testing::server::init_test_server;
use ya_net_server::testing::ClientBuilder;

#[serial_test::serial]
async fn test_single_packet_forward() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let client1 = ClientBuilder::from_server(&wrapper.server)
        .with_secret(generate())
        .connect()
        .build()
        .await
        .unwrap();
    let client2 = ClientBuilder::from_server(&wrapper.server)
        .with_secret(generate())
        .connect()
        .build()
        .await
        .unwrap();

    let _node1_id = client1.id().await;
    let _node2_id = client2.id().await;

    Ok(())
}
