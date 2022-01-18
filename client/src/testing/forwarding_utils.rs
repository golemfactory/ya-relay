use anyhow::{bail, Context};
use futures::{SinkExt, StreamExt};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::client::ForwardSender;
use crate::Client;

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
        let received = received.clone();
        async move {
            rx.for_each(|item| {
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
    let mut tx = match mode {
        Mode::Reliable => sender_client.forward(receiver_client.node_id()).await?,
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

    tx.send(vec![1u8]).await?;

    tokio::time::delay_for(Duration::from_millis(100)).await;

    if !received.load(SeqCst) {
        bail!("Data not received.")
    }

    // Clear receiver for further usage.
    received.store(false, SeqCst);
    Ok(tx)
}
