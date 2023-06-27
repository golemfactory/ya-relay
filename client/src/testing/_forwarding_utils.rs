use anyhow::{bail, Context};
use futures::StreamExt;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::_client::Client;
use crate::_transport_sender::{ForwardSender, GenericSender};

pub enum Mode {
    Reliable,
    Unreliable,
}

pub fn spawn_receive<T: std::fmt::Debug + 'static>(
    label: &'static str,
    received: Rc<AtomicBool>,
    rx: mpsc::UnboundedReceiver<T>,
) {
    println!("Spawning {} receiver", label);

    tokio::task::spawn_local({
        let received = received;
        async move {
            UnboundedReceiverStream::new(rx)
                .for_each(|item| {
                    let received = received.clone();
                    async move {
                        println!("{} received {:?}", label, item);
                        received.clone().store(true, SeqCst)
                    }
                })
                .await;
        }
    });
}

pub async fn spawn_receive_for_client(
    client: &Client,
    label: &'static str,
) -> anyhow::Result<Rc<AtomicBool>> {
    println!("{}: [{}]", label, client.node_id());

    let received = Rc::new(AtomicBool::new(false));
    let rx = client
        .forward_receiver()
        .await
        .context("no forward receiver")?;

    spawn_receive(label, received.clone(), rx);

    Ok(received)
}

/// Assign result to variable if you want to keep TCP connection alive.
pub async fn check_forwarding(
    sender_client: &Client,
    receiver_client: &Client,
    received: Rc<AtomicBool>,
    mode: Mode,
) -> anyhow::Result<ForwardSender> {
    // Clear receiver before usage.
    received.store(false, SeqCst);

    let mut tx = match mode {
        Mode::Reliable => {
            sender_client
                .forward_reliable(receiver_client.node_id())
                .await?
        }
        Mode::Unreliable => {
            sender_client
                .forward_unreliable(receiver_client.node_id())
                .await?
        }
    };

    println!(
        "Sending forward packets to [{}].",
        receiver_client.node_id()
    );

    // Avoid lazy initialization. If we aren't connected now, 100ms sleep is not enough in most cases.
    tx.connect().await?;
    tx.send(vec![1u8].into()).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    if !received.load(SeqCst) {
        bail!("Data not received.")
    }

    // Clear receiver for further usage.
    received.store(false, SeqCst);
    Ok(tx)
}

pub async fn check_broadcast(
    sender_client: &Client,
    receiver_client: &Client,
    received: Rc<AtomicBool>,
    nodes_count: u32,
) -> anyhow::Result<()> {
    // Clear receiver before usage.
    received.store(false, SeqCst);

    println!(
        "Sending forward packets to [{}].",
        receiver_client.node_id()
    );

    sender_client.broadcast(vec![1u8], nodes_count).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    if !received.load(SeqCst) {
        bail!("Data not received.")
    }

    // Clear receiver for further usage.
    received.store(false, SeqCst);
    Ok(())
}
