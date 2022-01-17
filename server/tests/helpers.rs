use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedReceiver;

use ya_relay_client::Client;
use ya_relay_server::testing::server::ServerWrapper;


/// TODO: Should be moved to ServerWrapper, but we don't want to import Client in Server crate.
pub async fn hack_make_ip_private(wrapper: &ServerWrapper, client: &Client) {
    let mut state = wrapper.server.state.write().await;

    let mut info = state.nodes.get_by_node_id(client.node_id()).unwrap();
    state.nodes.remove_session(info.info.slot);

    // Server won't return any endpoints, so Client won't try to connect directly.
    info.info.endpoints = vec![];
    state.nodes.register(info)
}

pub fn spawn_receive<T: std::fmt::Debug + 'static>(
    label: &'static str,
    received: Rc<AtomicBool>,
    rx: UnboundedReceiver<T>,
) {
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
