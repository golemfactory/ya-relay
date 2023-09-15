mod common;

use std::rc::Rc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use anyhow::Context;
use futures::StreamExt;
use itertools::Itertools;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use ya_relay_client::channels::Forwarded;
use ya_relay_client::{Client, ClientBuilder, FailFast};
use ya_relay_core::testing::TestServerWrapper;
use ya_relay_core::NodeId;
use ya_relay_server::testing::server::{init_test_server, ServerWrapper};

async fn start_clients(wrapper: &ServerWrapper, count: u32) -> Vec<Client> {
    let mut clients = vec![];
    for _ in 0..count {
        clients.push(
            ClientBuilder::from_url(wrapper.url())
                .connect(FailFast::Yes)
                .build()
                .await
                .unwrap(),
        )
    }
    clients
}

#[serial_test::serial]
async fn test_neighbourhood() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let clients = start_clients(&wrapper, 13).await;

    let node_id = clients[0].node_id();
    let session = clients[0].clone();

    let ids: Vec<NodeId> = session.neighbours(5).await.unwrap().into_iter().collect();

    // Node itself isn't returned in it's neighbourhood.
    assert!(!ids.contains(&node_id));

    // No duplicated nodes in neighbourhood.
    assert_eq!(ids.len(), 5);
    assert_eq!(ids.len(), ids.clone().into_iter().unique().count());

    // If no new nodes appeared or disappeared, server should return the same neighbourhood.
    let ids2: Vec<NodeId> = session.neighbours(5).await.unwrap().into_iter().collect();

    assert_eq!(ids, ids2);

    // When we take bigger neighbourhood it should contain smaller neighbouthood.
    let ids3: Vec<NodeId> = session.neighbours(8).await.unwrap().into_iter().collect();

    assert!(ids2.iter().all(|item| ids3.contains(item)));
    Ok(())
}

#[serial_test::serial]
async fn test_neighbourhood_too_big_neighbourhood_request() -> anyhow::Result<()> {
    let wrapper = init_test_server().await.unwrap();
    let clients = start_clients(&wrapper, 3).await;

    let node_id = clients[0].node_id();
    let session = clients[0].clone();

    let ids: Vec<NodeId> = session.neighbours(5).await.unwrap().into_iter().collect();

    // Node itself isn't returned in it's neighbourhood.
    assert!(!ids.contains(&node_id));

    // Node neighbourhood consists of all nodes beside requesting node.
    assert_eq!(ids.len(), 2);
    Ok(())
}

#[serial_test::serial]
async fn test_broadcast() -> anyhow::Result<()> {
    const NEIGHBOURHOOD_SIZE: u32 = 8;
    let wrapper = init_test_server().await.unwrap();
    let mut clients = start_clients(&wrapper, NEIGHBOURHOOD_SIZE + 1).await;

    fn spawn_receive(received: Rc<AtomicUsize>, rx: mpsc::UnboundedReceiver<Forwarded>) {
        tokio::task::spawn_local({
            let received = received;
            async move {
                UnboundedReceiverStream::new(rx)
                    .for_each(|item| {
                        let received = received.clone();
                        async move {
                            println!("received {:?}", item);
                            received.clone().fetch_add(item.payload.len(), SeqCst);
                        }
                    })
                    .await;
            }
        });
    }
    let mut received = vec![];
    let broadcasting_client = clients.pop().unwrap();

    for client in clients {
        let counter = Rc::new(AtomicUsize::new(0));
        received.push(counter.clone());
        let rx = client
            .forward_receiver()
            .await
            .context("no forward receiver")?;
        spawn_receive(counter, rx);
    }

    let data = vec![1_u8];
    broadcasting_client
        .broadcast(data, NEIGHBOURHOOD_SIZE)
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    for receiver in received {
        assert_eq!(receiver.load(SeqCst), 1);
    }
    Ok(())
}
